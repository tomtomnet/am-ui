//! A text of which the first lines are shown, with "more" to read the rest in place (the
//! description of a playlist). The button is there only while the text is longer than its lines.
//!
//! The lines keep a length that reads well however wide the page is. The widget does that
//! itself (it takes the width it is given and fills only part of it) because its height depends
//! on the width of its lines: an alignment that narrows it after it was measured would leave it
//! too little room.

use std::cell::Cell;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, pango};

const BUTTON_GAP: i32 = 2;
/// The longest line, in characters of average width.
const LINE_CHARS: i32 = 64;

mod imp {
    use super::*;

    pub struct Excerpt {
        pub label: gtk::Label,
        pub more: gtk::Button,
        pub lines: Cell<i32>,
        pub open: Cell<bool>,
    }

    impl Default for Excerpt {
        fn default() -> Self {
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            label.set_wrap(true);
            label.set_wrap_mode(pango::WrapMode::WordChar);
            label.set_ellipsize(pango::EllipsizeMode::End);
            let more = gtk::Button::with_label("more");
            more.add_css_class("flat");
            more.add_css_class("excerpt-more");
            Excerpt { label, more, lines: Cell::new(3), open: Cell::new(false) }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Excerpt {
        const NAME: &'static str = "AmExcerpt";
        type Type = super::Excerpt;
        type ParentType = gtk::Widget;

        fn class_init(class: &mut Self::Class) {
            class.set_css_name("excerpt");
        }
    }

    impl ObjectImpl for Excerpt {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            self.label.set_parent(&*obj);
            self.more.set_parent(&*obj);
            let weak = obj.downgrade();
            self.more.connect_clicked(move |_| {
                if let Some(excerpt) = weak.upgrade() {
                    excerpt.set_open(!excerpt.imp().open.get());
                }
            });
        }

        fn dispose(&self) {
            self.label.unparent();
            self.more.unparent();
        }
    }

    impl Excerpt {
        /// The width of the text in a widget of `width` (negative: as wide as it likes).
        fn text_width(&self, width: i32) -> i32 {
            let metrics = self.label.pango_context().metrics(None, None);
            let longest = (metrics.approximate_char_width() * LINE_CHARS / pango::SCALE).max(1);
            if width < 0 { longest } else { width.min(longest) }
        }

        /// Whether the text needs more than its lines at `width`.
        fn is_long(&self, width: i32) -> bool {
            let layout = self.label.layout().copy();
            layout.set_width(width * pango::SCALE);
            layout.set_height(-1);
            layout.set_ellipsize(pango::EllipsizeMode::None);
            layout.line_count() > self.lines.get()
        }

        fn has_button(&self, width: i32) -> bool {
            self.open.get() || self.is_long(width)
        }
    }

    impl WidgetImpl for Excerpt {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Horizontal {
                let (label_minimum, label_natural, _, _) = self.label.measure(orientation, -1);
                let (button_minimum, _, _, _) = self.more.measure(orientation, -1);
                let minimum = label_minimum.max(button_minimum);
                return (minimum, label_natural.min(self.text_width(-1)).max(minimum), -1, -1);
            }
            let width = self.text_width(for_size);
            let (_, mut height, _, _) = self.label.measure(orientation, width);
            if self.has_button(width) {
                height += BUTTON_GAP + self.more.measure(orientation, -1).1;
            }
            (height, height, -1, -1)
        }

        fn size_allocate(&self, width: i32, _height: i32, _baseline: i32) {
            let width = self.text_width(width);
            let (_, label_height, _, _) = self.label.measure(gtk::Orientation::Vertical, width);
            self.label.size_allocate(&gtk::Allocation::new(0, 0, width, label_height), -1);
            let shown = self.has_button(width);
            self.more.set_child_visible(shown);
            if shown {
                let (_, button_width, _, _) = self.more.measure(gtk::Orientation::Horizontal, -1);
                let (_, button_height, _, _) = self.more.measure(gtk::Orientation::Vertical, button_width);
                let allocation =
                    gtk::Allocation::new(0, label_height + BUTTON_GAP, button_width.min(width), button_height);
                self.more.size_allocate(&allocation, -1);
            }
        }
    }
}

glib::wrapper! {
    pub struct Excerpt(ObjectSubclass<imp::Excerpt>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Excerpt {
    /// `text`, of which `lines` lines are shown until the reader asks for more.
    pub fn new(text: &str, lines: i32) -> Excerpt {
        let excerpt: Excerpt = glib::Object::new();
        let imp = excerpt.imp();
        imp.lines.set(lines.max(1));
        imp.label.set_text(text);
        imp.label.set_lines(lines.max(1));
        excerpt
    }

    /// Show the whole text, or its first lines again.
    pub fn set_open(&self, open: bool) {
        let imp = self.imp();
        imp.open.set(open);
        imp.label.set_lines(if open { -1 } else { imp.lines.get() });
        imp.label.set_ellipsize(if open { pango::EllipsizeMode::None } else { pango::EllipsizeMode::End });
        imp.more.set_label(if open { "less" } else { "more" });
        self.queue_resize();
    }
}
