//! One song in a list: artwork or track number, title (with a mark when the song is explicit)
//! and artist, album, favourite star, "more" button and duration.
//!
//! Built from ordinary widgets (labels and buttons) placed by hand in `size_allocate`, so the
//! columns line up from row to row without size groups. What holds text is measured in the
//! fonts of the labels (`Metrics`): a line of the title and one of the line under it, the widest
//! track number and duration. The row grows with the desktop's text size, the artwork with the
//! two lines beside it, and every row of a list has the same height whatever its text says.
//!
//! Pointing at the row shows the play glyph over the artwork or number and the two buttons; the
//! song that is playing has its title in the accent colour and a small level meter in place of
//! the number. The artist under the title and the album are links to their pages; a click
//! anywhere else does what a click on a row of the list does.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use amui::types::Track;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk, pango};

use crate::actions::{has_album, has_artist};
use crate::widgets::thumb::Thumb;
use crate::widgets::{clock, font_serial, label_line, label_width, make_link, scaled, set_link};

const PAD: i32 = 14;
/// The smallest side of the artwork: it grows with the two lines of text beside it.
const ART: i32 = 40;
/// Room above and below the artwork, and above and below the two lines of text beside it (in
/// all, both sides together).
const ART_ROOM: i32 = 14;
const TEXT_ROOM: i32 = 22;
/// Room above and below the lines of a numbered row (the songs of an album).
const NUMBERED_ROOM: i32 = 12;
/// The columns of the track numbers and of the durations at the usual text size; they widen
/// with the text.
const NUMBER_WIDTH: i32 = 26;
const DURATION_WIDTH: i32 = 48;
const BUTTON: i32 = 32;
const GAP: i32 = 12;
/// Between the title of an explicit song and its mark.
const EXPLICIT_GAP: i32 = 6;
/// The text needs this much room for the album to get a column of its own (at the usual text
/// size).
const ALBUM_ROOM: i32 = 420;
/// The narrowest row, and the least room it leaves the title.
const MIN_WIDTH: i32 = 320;
const TITLE_ROOM: i32 = 40;
/// How long a row that "Go to Song" found stays lit up, and how long its light takes to fade
/// (style.css: `trackrow.fading`).
const FLASH_MS: u64 = 700;
const FADE_MS: u64 = 1000;

/// How a list shows its songs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RowStyle {
    /// Track numbers instead of artwork (album pages).
    pub numbered: bool,
    /// The artist under the title.
    pub artist: bool,
    /// The album in a column of its own, when the row is wide enough.
    pub album: bool,
}

impl RowStyle {
    /// Songs of mixed albums: artwork, artist and album.
    pub const LIST: RowStyle = RowStyle { numbered: false, artist: true, album: true };
    /// The songs of one album.
    pub const ALBUM: RowStyle = RowStyle { numbered: true, artist: false, album: false };
}

/// What the line under the title shows. It is a link to what it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowLine {
    Artist,
    Album,
}

/// The sizes of a row that come from its fonts, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Metrics {
    /// A line of the title, and a line of the text under it.
    pub title: i32,
    pub line: i32,
    /// The widest track number (three digits) and duration ("00:00").
    pub number: i32,
    pub duration: i32,
}

impl Metrics {
    /// The side of the artwork: as tall as the two lines of text beside it, with the room the
    /// text has more than the artwork.
    pub fn art(&self) -> i32 {
        (self.title + self.line + TEXT_ROOM - ART_ROOM).max(ART)
    }

    /// The height of a row of `style`. Every row of a list has it, whether a line shows under
    /// its title or not (on an album's page, the songs with a guest have one).
    pub fn height(&self, style: RowStyle) -> i32 {
        let text = self.title + self.line;
        if style.numbered { text + NUMBERED_ROOM } else { (self.art() + ART_ROOM).max(text + TEXT_ROOM) }
    }

    /// The narrowest row of `style`: its columns side by side, with a little room for the title.
    pub fn width(&self, style: RowStyle) -> i32 {
        let lead = if style.numbered { self.number_column() } else { self.art() };
        let row = PAD + lead + GAP + TITLE_ROOM + GAP + 2 * BUTTON + 4 + self.duration_column() + PAD;
        row.max(MIN_WIDTH)
    }

    pub fn number_column(&self) -> i32 {
        NUMBER_WIDTH.max(self.number)
    }

    /// Room for "00:00" and a little more before it.
    pub fn duration_column(&self) -> i32 {
        DURATION_WIDTH.max(self.duration + DURATION_WIDTH / 4)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RowAction {
    /// Play from this song.
    Play,
    ToggleFavorite,
    /// The context menu, at this point of the row.
    Menu(f64, f64),
    /// The artist's page, or the album's (a click on the link).
    OpenArtist,
    OpenAlbum,
}

pub type RowHandler = Rc<dyn Fn(u32, &Track, RowAction, &TrackRow)>;

mod imp {
    use super::*;

    pub struct TrackRow {
        pub style: Cell<RowStyle>,
        pub track: RefCell<Option<Track>>,
        pub position: Cell<u32>,
        pub handler: RefCell<Option<RowHandler>>,
        pub thumb: Thumb,
        pub number: gtk::Label,
        pub title: gtk::Label,
        pub explicit: gtk::Image,
        /// The line under the title: the artist, or the album.
        pub line: gtk::Label,
        pub album: gtk::Label,
        pub star: gtk::Button,
        pub more: gtk::Button,
        pub duration: gtk::Label,
        pub hover: Cell<bool>,
        pub playing: Cell<bool>,
        pub favorite: Cell<bool>,
        /// What the line under the title shows (`None`: there is no line).
        pub shown_line: Cell<Option<RowLine>>,
        /// The metrics of the fonts, with the `font_serial` they were measured at.
        pub metrics: Cell<Option<(u64, Metrics)>>,
        /// The play glyph's place: the artwork, or the column of the numbers.
        pub lead: Cell<graphene::Rect>,
        /// The cell of the list the row is in, which knows the row's place when rows come and
        /// go above it.
        pub cell: glib::WeakRef<gtk::ListItem>,
        /// Counts the flashes: a timer acts for its own flash only.
        pub flashes: Cell<u32>,
    }

    impl Default for TrackRow {
        fn default() -> Self {
            let label = |class: &str| {
                let label = gtk::Label::new(None);
                label.set_xalign(0.0);
                label.set_ellipsize(pango::EllipsizeMode::End);
                label.set_single_line_mode(true);
                label.add_css_class(class);
                label
            };
            let button = |icon: &str, tip: &str| {
                let button = gtk::Button::from_icon_name(icon);
                button.add_css_class("flat");
                button.add_css_class("circular");
                button.add_css_class("hover-only");
                button.set_tooltip_text(Some(tip));
                button.set_focus_on_click(false);
                button
            };
            let number = label("row-number");
            number.set_xalign(0.5);
            number.add_css_class("numeric");
            number.add_css_class("dim-label");
            let duration = label("row-duration");
            duration.set_xalign(1.0);
            duration.add_css_class("numeric");
            duration.add_css_class("dim-label");
            let line = label("row-artist");
            line.add_css_class("dim-label");
            let album = label("row-album");
            album.add_css_class("dim-label");
            // its size is the style's (-gtk-icon-size): it grows with the title
            let explicit = gtk::Image::from_icon_name("am-explicit-symbolic");
            explicit.set_icon_size(gtk::IconSize::Inherit);
            explicit.add_css_class("row-explicit");
            explicit.set_tooltip_text(Some("Explicit"));
            explicit.set_visible(false);
            TrackRow {
                style: Cell::default(),
                track: RefCell::default(),
                position: Cell::new(0),
                handler: RefCell::default(),
                thumb: Thumb::new(ART, 5.0),
                number,
                title: label("row-title"),
                explicit,
                line,
                album,
                star: button("non-starred-symbolic", "Favorite"),
                more: button("view-more-horizontal-symbolic", "More"),
                duration,
                hover: Cell::new(false),
                playing: Cell::new(false),
                favorite: Cell::new(false),
                shown_line: Cell::new(None),
                metrics: Cell::new(None),
                lead: Cell::new(graphene::Rect::zero()),
                cell: glib::WeakRef::new(),
                flashes: Cell::new(0),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TrackRow {
        const NAME: &'static str = "AmTrackRow";
        type Type = super::TrackRow;
        type ParentType = gtk::Widget;

        fn class_init(class: &mut Self::Class) {
            class.set_css_name("trackrow");
        }
    }

    impl ObjectImpl for TrackRow {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            for child in [
                self.thumb.upcast_ref::<gtk::Widget>(),
                self.number.upcast_ref(),
                self.title.upcast_ref(),
                self.explicit.upcast_ref(),
                self.line.upcast_ref(),
                self.album.upcast_ref(),
                self.star.upcast_ref(),
                self.more.upcast_ref(),
                self.duration.upcast_ref(),
            ] {
                child.set_parent(&*obj);
            }
            let weak = obj.downgrade();
            self.star.connect_clicked(move |_| {
                if let Some(row) = weak.upgrade() {
                    row.emit(RowAction::ToggleFavorite);
                }
            });
            let weak = obj.downgrade();
            self.more.connect_clicked(move |button| {
                if let Some(row) = weak.upgrade() {
                    let at = button
                        .compute_point(&row, &graphene::Point::new(BUTTON as f32 / 2.0, BUTTON as f32))
                        .unwrap_or_default();
                    row.emit(RowAction::Menu(at.x() as f64, at.y() as f64));
                }
            });
            let weak = obj.downgrade();
            make_link(&self.line, move || {
                if let Some(row) = weak.upgrade() {
                    match row.imp().shown_line.get() {
                        Some(RowLine::Artist) => row.emit(RowAction::OpenArtist),
                        Some(RowLine::Album) => row.emit(RowAction::OpenAlbum),
                        None => {}
                    }
                }
            });
            let weak = obj.downgrade();
            make_link(&self.album, move || {
                if let Some(row) = weak.upgrade() {
                    row.emit(RowAction::OpenAlbum);
                }
            });

            let motion = gtk::EventControllerMotion::new();
            let weak = obj.downgrade();
            motion.connect_enter(move |_, _, _| {
                if let Some(row) = weak.upgrade() {
                    row.imp().hover.set(true);
                    row.queue_draw();
                }
            });
            let weak = obj.downgrade();
            motion.connect_leave(move |_| {
                if let Some(row) = weak.upgrade() {
                    row.imp().hover.set(false);
                    row.queue_draw();
                }
            });
            obj.add_controller(motion);

            let click = gtk::GestureClick::new();
            click.set_button(0);
            let weak = obj.downgrade();
            click.connect_pressed(move |gesture, _, x, y| {
                let Some(row) = weak.upgrade() else { return };
                match gesture.current_button() {
                    gdk::BUTTON_SECONDARY => {
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                        row.emit(RowAction::Menu(x, y));
                    }
                    // the artwork or number is a play button
                    gdk::BUTTON_PRIMARY if row.on_lead(x) => {
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                        row.emit(RowAction::Play);
                    }
                    _ => {}
                }
            });
            obj.add_controller(click);
        }

        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for TrackRow {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::ConstantSize
        }

        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let metrics = self.obj().metrics();
            if orientation == gtk::Orientation::Horizontal {
                (metrics.width(self.style.get()), 720, -1, -1)
            } else {
                let height = metrics.height(self.style.get());
                (height, height, -1, -1)
            }
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let style = self.style.get();
            let metrics = self.obj().metrics();
            // every child is measured before it is placed, as GTK asks: (natural width, height)
            let measure = |child: &gtk::Widget| {
                let (_, natural, _, _) = child.measure(gtk::Orientation::Horizontal, -1);
                (natural, child.measure(gtk::Orientation::Vertical, -1).1)
            };
            // `child` in the middle of the line (or the row) from `top` that is `high` tall
            let put = |child: &gtk::Widget, x: i32, wide: i32, top: i32, high: i32| {
                let (_, own) = measure(child);
                child.size_allocate(&gtk::Allocation::new(x, top + (high - own) / 2, wide.max(1), own), -1);
            };

            let art = metrics.art();
            let lead = if style.numbered { metrics.number_column() } else { art };
            self.lead.set(graphene::Rect::new(PAD as f32, ((height - art) / 2) as f32, lead as f32, art as f32));
            self.thumb.set_child_visible(!style.numbered);
            self.number.set_child_visible(style.numbered);
            if style.numbered {
                put(self.number.upcast_ref(), PAD, lead, 0, height);
            } else {
                let _ = measure(self.thumb.upcast_ref());
                self.thumb.size_allocate(&gtk::Allocation::new(PAD, (height - art) / 2, art, art), -1);
            }

            // a duration of an hour or more takes what it needs from the buttons' side
            let (duration_natural, _) = measure(self.duration.upcast_ref());
            let duration_width = metrics.duration_column().max(duration_natural);
            let duration_x = width - PAD - duration_width;
            let more_x = duration_x - BUTTON - 4;
            let star_x = more_x - BUTTON;
            put(self.duration.upcast_ref(), duration_x, duration_width, 0, height);
            put(self.more.upcast_ref(), more_x, BUTTON, 0, height);
            put(self.star.upcast_ref(), star_x, BUTTON, 0, height);

            let text_x = PAD + lead + GAP;
            let free = (star_x - GAP - text_x).max(TITLE_ROOM);
            let with_album = style.album && free > scaled(&*self.obj(), ALBUM_ROOM);
            let title_width = if with_album { free * 58 / 100 - GAP } else { free };
            self.album.set_child_visible(with_album);
            if with_album {
                // a link: no wider than its text
                let (natural, _) = measure(self.album.upcast_ref());
                put(self.album.upcast_ref(), text_x + free * 58 / 100, natural.min(free * 42 / 100), 0, height);
            }

            // the mark of an explicit song follows its title: the title takes what it needs
            let (title_natural, _) = measure(self.title.upcast_ref());
            let marked = self.explicit.is_visible();
            let (mark, _) = if marked { measure(self.explicit.upcast_ref()) } else { (0, 0) };
            let title_shown =
                if marked { title_natural.min(title_width - mark - EXPLICIT_GAP).max(1) } else { title_width };
            // the lines take the height of their fonts, in the middle of the row
            let lined = self.shown_line.get().is_some();
            self.line.set_child_visible(lined);
            let (title_top, title_high) = if lined {
                ((height - metrics.title - metrics.line) / 2, metrics.title)
            } else {
                ((height - metrics.title) / 2, metrics.title)
            };
            put(self.title.upcast_ref(), text_x, title_shown, title_top, title_high);
            if marked {
                put(self.explicit.upcast_ref(), text_x + title_shown + EXPLICIT_GAP, mark, title_top, title_high);
            }
            if lined {
                let (natural, _) = measure(self.line.upcast_ref());
                put(self.line.upcast_ref(), text_x, natural.min(title_width), title_top + title_high, metrics.line);
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let style = self.style.get();
            let colour = obj.color();
            let lead = self.lead.get();
            // the glyphs grow with the artwork
            let scale = lead.height() / ART as f32;
            let (hover, playing) = (self.hover.get(), self.playing.get());
            if style.numbered {
                if playing && !hover {
                    // in the colour the style gives the title of the song that is playing
                    draw_meter(snapshot, &lead, &self.title.color(), scale);
                } else if hover {
                    draw_play(snapshot, &lead, &colour, scale);
                }
            }
            let number: &gtk::Widget = self.number.upcast_ref();
            let mut child = obj.first_child();
            while let Some(widget) = child {
                // the number gives way to the glyphs
                let covered = widget == *number && (hover || playing);
                if widget.is_child_visible() && !covered {
                    obj.snapshot_child(&widget, snapshot);
                }
                child = widget.next_sibling();
            }
            if !style.numbered && (hover || playing) {
                let shape = gsk::RoundedRect::from_rect(lead, 5.0 * scale);
                snapshot.push_rounded_clip(&shape);
                snapshot.append_color(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.45), &lead);
                snapshot.pop();
                let white = gdk::RGBA::new(1.0, 1.0, 1.0, 1.0);
                if playing && !hover {
                    draw_meter(snapshot, &lead, &white, scale);
                } else {
                    draw_play(snapshot, &lead, &white, scale);
                }
            }
        }
    }
}

/// The play glyph in the middle of `area`, `scale` times its usual size.
pub(crate) fn draw_play(snapshot: &gtk::Snapshot, area: &graphene::Rect, colour: &gdk::RGBA, scale: f32) {
    let (cx, cy) = (area.x() + area.width() / 2.0, area.y() + area.height() / 2.0);
    let builder = gsk::PathBuilder::new();
    builder.move_to(cx - 4.5 * scale, cy - 7.0 * scale);
    builder.line_to(cx + 7.5 * scale, cy);
    builder.line_to(cx - 4.5 * scale, cy + 7.0 * scale);
    builder.close();
    snapshot.append_fill(&builder.to_path(), gsk::FillRule::Winding, colour);
}

/// Three bars of a level meter, `scale` times their usual size: the mark of the song that is
/// playing.
pub(crate) fn draw_meter(snapshot: &gtk::Snapshot, area: &graphene::Rect, colour: &gdk::RGBA, scale: f32) {
    let (cx, cy) = (area.x() + area.width() / 2.0, area.y() + area.height() / 2.0);
    for (dx, bar) in [(-5.0, 8.0), (0.0, 14.0), (5.0, 10.0)] {
        let (dx, bar, half) = (dx * scale, bar * scale, 1.5 * scale);
        let rect = graphene::Rect::new(cx + dx - half, cy + 7.0 * scale - bar, 2.0 * half, bar);
        snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(rect, half));
        snapshot.append_color(colour, &rect);
        snapshot.pop();
    }
}

glib::wrapper! {
    pub struct TrackRow(ObjectSubclass<imp::TrackRow>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl TrackRow {
    pub fn new(style: RowStyle, handler: RowHandler) -> TrackRow {
        let row: TrackRow = glib::Object::new();
        row.imp().style.set(style);
        row.imp().handler.replace(Some(handler));
        row
    }

    pub fn track(&self) -> Option<Track> {
        self.imp().track.borrow().clone()
    }

    /// The row is the child of `cell` in a list view: its place is the cell's.
    pub fn set_cell(&self, cell: &gtk::ListItem) {
        self.imp().cell.set(Some(cell));
    }

    /// The row's place in its list: where its cell is now (rows may have come or gone above it
    /// since it was given its song), else where it was given its song.
    fn position(&self) -> u32 {
        let cell = self.imp().cell.upgrade().map(|cell| cell.position());
        cell.filter(|position| *position != gtk::INVALID_LIST_POSITION).unwrap_or(self.imp().position.get())
    }

    /// Light the row up in the accent colour for a moment, then let the colour fade out
    /// ("Go to Song" found its song; style.css has the colours and the fade).
    pub fn flash(&self) {
        let imp = self.imp();
        let flash = imp.flashes.get() + 1;
        imp.flashes.set(flash);
        self.remove_css_class("fading");
        self.add_css_class("found");
        let weak = self.downgrade();
        glib::timeout_add_local_once(Duration::from_millis(FLASH_MS), move || {
            let Some(row) = weak.upgrade().filter(|row| row.imp().flashes.get() == flash) else { return };
            row.add_css_class("fading");
            row.remove_css_class("found");
            let weak = row.downgrade();
            glib::timeout_add_local_once(Duration::from_millis(FADE_MS), move || {
                if let Some(row) = weak.upgrade().filter(|row| row.imp().flashes.get() == flash) {
                    row.remove_css_class("fading");
                }
            });
        });
    }

    /// End a flash at once, without a fade (the row is given another song).
    fn stop_flash(&self) {
        let imp = self.imp();
        imp.flashes.set(imp.flashes.get() + 1);
        self.remove_css_class("found");
        self.remove_css_class("fading");
    }

    /// The sizes that come from the fonts of the labels, measured again when one of those fonts
    /// changes.
    pub fn metrics(&self) -> Metrics {
        let imp = self.imp();
        let serial = font_serial(&[
            imp.title.upcast_ref(),
            imp.line.upcast_ref(),
            imp.number.upcast_ref(),
            imp.duration.upcast_ref(),
        ]);
        if let Some((measured, metrics)) = imp.metrics.get()
            && measured == serial
        {
            return metrics;
        }
        let metrics = Metrics {
            title: label_line(&imp.title),
            line: label_line(&imp.line),
            number: label_width(&imp.number, "000"),
            duration: label_width(&imp.duration, "00:00"),
        };
        imp.metrics.set(Some((serial, metrics)));
        metrics
    }

    /// Whether `x` is on the artwork or the number, which play the song on a single click.
    pub fn on_lead(&self, x: f64) -> bool {
        let lead = self.imp().lead.get();
        x < (lead.x() + lead.width()) as f64 + (GAP / 2) as f64
    }

    /// Show `track`, the song at `position` of its list.
    pub fn bind(&self, position: u32, track: &Track, playing: bool, favorite: bool) {
        let imp = self.imp();
        let style = imp.style.get();
        imp.position.set(position);
        let before = imp.track.replace(Some(track.clone()));
        if before.is_none_or(|before| before.key() != track.key()) {
            self.stop_flash();
        }
        imp.title.set_text(&track.name);
        imp.explicit.set_visible(track.content_rating.as_deref() == Some("explicit"));
        imp.album.set_text(&track.album);
        set_link(&imp.album, has_album(track));
        imp.duration.set_text(&track.duration_ms.map(|ms| clock(ms as f64 / 1000.0)).unwrap_or_default());
        if style.numbered {
            imp.number.set_text(&track.track_number.map(|n| n.to_string()).unwrap_or_default());
        } else {
            imp.thumb.set_artwork(track.artwork.as_ref());
        }
        if track.playable() {
            self.remove_css_class("unavailable");
        } else {
            self.add_css_class("unavailable");
        }
        self.set_line(style.artist.then_some(RowLine::Artist));
        self.set_playing(playing);
        self.set_favorite(favorite);
        self.queue_draw();
    }

    /// Show `line` under the title in place of what the row's style says (`None`: no line). It
    /// holds for the song the row shows now.
    pub fn set_line(&self, line: Option<RowLine>) {
        let imp = self.imp();
        if let (Some(line), Some(track)) = (line, imp.track.borrow().as_ref()) {
            let (text, linked) = match line {
                RowLine::Artist => (&track.artist, has_artist(track)),
                RowLine::Album => (&track.album, has_album(track)),
            };
            imp.line.set_text(text);
            set_link(&imp.line, linked);
        }
        if imp.shown_line.replace(line) != line {
            self.queue_allocate();
        }
    }

    pub fn unbind(&self) {
        self.imp().track.replace(None);
        self.imp().thumb.set_artwork(None);
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

    pub fn set_favorite(&self, favorite: bool) {
        let imp = self.imp();
        imp.favorite.set(favorite);
        imp.star.set_icon_name(if favorite { "starred-symbolic" } else { "non-starred-symbolic" });
        imp.star.set_tooltip_text(Some(if favorite { "Undo Favorite" } else { "Favorite" }));
        if favorite {
            imp.star.remove_css_class("hover-only");
            imp.star.add_css_class("favorite");
        } else {
            imp.star.add_css_class("hover-only");
            imp.star.remove_css_class("favorite");
        }
    }

    fn emit(&self, action: RowAction) {
        let handler = self.imp().handler.borrow().clone();
        let track = self.imp().track.borrow().clone();
        if let (Some(handler), Some(track)) = (handler, track) {
            handler(self.position(), &track, action, self);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::widgets::item_row::ItemRow;
    use crate::widgets::song_cell::{SongCell, SongHandler};
    use crate::widgets::tests::{Stage, assert_text_fits};

    /// What a row's fonts measure at this desktop's usual text size (10 points), and at twice it.
    const USUAL: Metrics = Metrics { title: 17, line: 15, number: 26, duration: 36 };
    const DOUBLE: Metrics = Metrics { title: 33, line: 30, number: 52, duration: 71 };

    #[test]
    fn rows_are_as_large_as_their_text() {
        // the usual text size: the sizes the rows were drawn with
        assert_eq!((USUAL.height(RowStyle::LIST), USUAL.height(RowStyle::ALBUM)), (54, 44));
        assert_eq!((USUAL.art(), USUAL.number_column(), USUAL.duration_column()), (40, 26, 48));
        assert_eq!(USUAL.width(RowStyle::LIST), 320);
        // twice as large: two lines and their room, artwork as tall as the lines, the widest
        // duration and number in their columns
        assert_eq!((DOUBLE.height(RowStyle::LIST), DOUBLE.art(), DOUBLE.height(RowStyle::ALBUM)), (85, 71, 75));
        assert!(DOUBLE.duration_column() > DOUBLE.duration && DOUBLE.number_column() >= DOUBLE.number);
        assert_eq!(DOUBLE.width(RowStyle::LIST), 320);
        // so large that the columns need more than the narrowest row
        let huge = Metrics { title: 50, line: 45, number: 80, duration: 110 };
        assert!(huge.width(RowStyle::LIST) > 320);
        // smaller than usual: the artwork and the columns keep their sizes
        let small = Metrics { title: 12, line: 11, number: 18, duration: 26 };
        assert_eq!((small.art(), small.height(RowStyle::LIST), small.duration_column()), (40, 54, 48));
    }

    /// Rows of songs, of a shelf and of albums at four text sizes, set while they are shown as
    /// the desktop's setting would be (part of `widgets::tests::widgets_on_a_display`).
    pub(crate) fn on_a_display() {
        // the application's style: the sizes of its labels
        let display = gdk::Display::default().expect("a display");
        let style = gtk::CssProvider::new();
        style.load_from_string(include_str!("../style.css"));
        gtk::style_context_add_provider_for_display(&display, &style, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);

        let track = Track {
            album: "Album A".into(),
            duration_ms: Some(3_765_000),
            track_number: Some(12),
            content_rating: Some("explicit".into()),
            ..Track::new(Some("1000001"), "Song A", "Artist A")
        };
        let handler: RowHandler = Rc::new(|_, _, _, _| {});
        let listed = TrackRow::new(RowStyle::LIST, handler.clone());
        listed.bind(0, &track, false, false);
        let numbered = TrackRow::new(RowStyle::ALBUM, handler);
        numbered.bind(0, &track, false, false);
        numbered.set_line(Some(RowLine::Artist));
        let shelf_handler: SongHandler = Rc::new(|_, _, _, _| {});
        let cell = SongCell::new(0, &track, shelf_handler);
        let album = ItemRow::new(Rc::new(|_, _, _, _| {}));
        let item = amui::types::Item::Album(amui::types::Album {
            release_date: Some("2019-03-08".into()),
            ..amui::types::Album::new(Some("1"), "Album A", "Artist A")
        });
        album.bind(Some(&item));
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        for row in [listed.upcast_ref::<gtk::Widget>(), numbered.upcast_ref(), cell.upcast_ref(), album.upcast_ref()] {
            column.append(row);
        }
        column.set_valign(gtk::Align::Start);
        let stage = Stage::new(&column, 900, 600);

        let settings = gtk::Settings::default().expect("a display has settings");
        let mut heights = Vec::new();
        for scale in [1.0, 1.25, 1.5, 2.0] {
            settings.set_gtk_xft_dpi((96.0 * 1024.0 * scale) as i32);
            stage.frames(6);
            for (row, what) in [(&listed, "a row of a list"), (&numbered, "a row of an album")] {
                let what = format!("{what} at {scale}");
                assert_text_fits(row.upcast_ref(), &what);
                assert_eq!(row.height(), row.metrics().height(row.imp().style.get()), "{what}");
                // a duration of an hour and more is whole, and the lines are one under the other
                let duration = &row.imp().duration;
                assert!(duration.width() >= duration.measure(gtk::Orientation::Horizontal, -1).1, "{what}");
                let place = |label: &gtk::Label| label.compute_bounds(row).expect("a label of the row");
                let (title, line) = (place(&row.imp().title), place(&row.imp().line));
                assert!(title.y() + title.height() <= line.y() + 1.0, "{what}: the title runs into the line under it");
            }
            assert_text_fits(cell.upcast_ref(), &format!("a song of a shelf at {scale}"));
            assert_text_fits(album.upcast_ref(), &format!("a row of albums at {scale}"));
            heights.push([listed.height(), numbered.height(), cell.height(), album.height()]);
        }
        settings.reset_property("gtk-xft-dpi");
        stage.frames(6);
        for pair in heights.windows(2) {
            assert!(pair[1].iter().zip(&pair[0]).all(|(after, before)| after > before), "{heights:?}");
        }
        // a larger font for the desktop (its size, not the text scaling): the rows follow it too
        let usual = listed.height();
        settings.set_gtk_font_name(Some("Sans 16"));
        stage.frames(6);
        assert!(listed.height() > usual, "{} at a larger font, {usual} before", listed.height());
        assert_text_fits(listed.upcast_ref(), "a row of a list in a larger font");
        settings.reset_property("gtk-font-name");
        gtk::style_context_remove_provider_for_display(&display, &style);
        stage.frames(4);
        // the artist under the title and the album are links (the song has ids for both)
        let imp = listed.imp();
        assert!(imp.line.has_css_class("text-link") && imp.album.has_css_class("text-link"));
        assert_eq!(imp.line.cursor().and_then(|cursor| cursor.name()).as_deref(), Some("pointer"));
        // a song without ids has no pages to lead to
        listed.bind(1, &Track::new(None, "Song B", "Artist B"), false, false);
        assert!(!imp.line.has_css_class("text-link") && imp.line.cursor().is_none());
    }
}
