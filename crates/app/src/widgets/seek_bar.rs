//! The progress of the current song: a thin bar that shows what has played and what is ready,
//! thickens under the pointer and can be dragged to seek.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

use crate::widgets::with_alpha;

const HEIGHT: i32 = 18;
const THIN: f32 = 4.0;
const THICK: f32 = 6.0;
const KNOB: f32 = 12.0;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct SeekBar {
        /// Played and ready parts, 0..1.
        pub fraction: Cell<f64>,
        pub buffered: Cell<f64>,
        pub hover: Cell<bool>,
        /// The fraction under the pointer while the bar is being dragged.
        pub dragging: Cell<Option<f64>>,
        pub on_seek: RefCell<Option<Rc<dyn Fn(f64)>>>,
        pub enabled: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SeekBar {
        const NAME: &'static str = "AmSeekBar";
        type Type = super::SeekBar;
        type ParentType = gtk::Widget;

        fn class_init(class: &mut Self::Class) {
            class.set_css_name("seekbar");
            class.set_accessible_role(gtk::AccessibleRole::Slider);
        }
    }

    impl ObjectImpl for SeekBar {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_hexpand(true);
            let motion = gtk::EventControllerMotion::new();
            let weak = obj.downgrade();
            motion.connect_enter(move |_, _, _| {
                if let Some(bar) = weak.upgrade() {
                    bar.imp().hover.set(true);
                    bar.queue_draw();
                }
            });
            let weak = obj.downgrade();
            motion.connect_leave(move |_| {
                if let Some(bar) = weak.upgrade() {
                    bar.imp().hover.set(false);
                    bar.queue_draw();
                }
            });
            obj.add_controller(motion);

            let drag = gtk::GestureDrag::new();
            let weak = obj.downgrade();
            drag.connect_drag_begin(move |gesture, x, _| {
                let Some(bar) = weak.upgrade() else { return };
                if !bar.imp().enabled.get() {
                    return;
                }
                gesture.set_state(gtk::EventSequenceState::Claimed);
                bar.imp().dragging.set(Some(bar.fraction_at(x)));
                bar.queue_draw();
            });
            let weak = obj.downgrade();
            drag.connect_drag_update(move |gesture, dx, _| {
                let Some(bar) = weak.upgrade() else { return };
                if bar.imp().dragging.get().is_some()
                    && let Some((start, _)) = gesture.start_point()
                {
                    bar.imp().dragging.set(Some(bar.fraction_at(start + dx)));
                    bar.queue_draw();
                }
            });
            let weak = obj.downgrade();
            drag.connect_drag_end(move |_, _, _| {
                let Some(bar) = weak.upgrade() else { return };
                if let Some(fraction) = bar.imp().dragging.take() {
                    bar.imp().fraction.set(fraction);
                    bar.queue_draw();
                    let handler = bar.imp().on_seek.borrow().clone();
                    if let Some(handler) = handler {
                        handler(fraction);
                    }
                }
            });
            obj.add_controller(drag);
        }
    }

    impl WidgetImpl for SeekBar {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Horizontal { (80, 320, -1, -1) } else { (HEIGHT, HEIGHT, -1, -1) }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let (width, height) = (obj.width() as f32, obj.height() as f32);
            let colour = obj.color();
            let active = self.enabled.get() && (self.hover.get() || self.dragging.get().is_some());
            let thick = if active { THICK } else { THIN };
            let y = (height - thick) / 2.0;
            let bar = |from: f32, to: f32, colour: &gdk::RGBA| {
                let rect = graphene::Rect::new(from, y, (to - from).max(0.0), thick);
                snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(
                    graphene::Rect::new(0.0, y, width, thick),
                    thick / 2.0,
                ));
                snapshot.append_color(colour, &rect);
                snapshot.pop();
            };
            bar(0.0, width, &with_alpha(&colour, 0.16));
            if !self.enabled.get() {
                return;
            }
            let fraction = self.dragging.get().unwrap_or(self.fraction.get()).clamp(0.0, 1.0) as f32;
            let ready = (fraction + self.buffered.get().clamp(0.0, 1.0) as f32).min(1.0);
            bar(fraction * width, ready * width, &with_alpha(&colour, 0.14));
            bar(0.0, fraction * width, &with_alpha(&colour, if active { 0.95 } else { 0.62 }));
            if active {
                let x = (fraction * width).clamp(KNOB / 2.0, width - KNOB / 2.0);
                let knob = graphene::Rect::new(x - KNOB / 2.0, (height - KNOB) / 2.0, KNOB, KNOB);
                let shape = gsk::RoundedRect::from_rect(knob, KNOB / 2.0);
                snapshot.append_outset_shadow(&shape, &gdk::RGBA::new(0.0, 0.0, 0.0, 0.35), 0.0, 1.0, 0.0, 3.0);
                snapshot.push_rounded_clip(&shape);
                snapshot.append_color(&gdk::RGBA::new(1.0, 1.0, 1.0, 1.0), &knob);
                snapshot.pop();
            }
        }
    }
}

glib::wrapper! {
    pub struct SeekBar(ObjectSubclass<imp::SeekBar>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for SeekBar {
    fn default() -> Self {
        SeekBar::new()
    }
}

impl SeekBar {
    pub fn new() -> SeekBar {
        glib::Object::new()
    }

    /// Called with the fraction (0..1) the user let go at.
    pub fn connect_seek(&self, handler: impl Fn(f64) + 'static) {
        self.imp().on_seek.replace(Some(Rc::new(handler)));
    }

    /// Whether there is a song to seek in.
    pub fn set_enabled(&self, enabled: bool) {
        if self.imp().enabled.replace(enabled) != enabled {
            self.imp().dragging.set(None);
            self.queue_draw();
        }
    }

    /// The fraction the user is dragging to, while they drag.
    pub fn dragging(&self) -> Option<f64> {
        self.imp().dragging.get()
    }

    /// Show `fraction` played and `buffered` ready beyond it; redraws only when a pixel changes.
    pub fn set_progress(&self, fraction: f64, buffered: f64) {
        let imp = self.imp();
        let width = self.width().max(1) as f64 * self.scale_factor() as f64;
        let moved = |old: f64, new: f64| (old * width).round() != (new * width).round();
        let changed = moved(imp.fraction.get(), fraction) || moved(imp.buffered.get(), buffered);
        imp.fraction.set(fraction);
        imp.buffered.set(buffered);
        if changed && imp.dragging.get().is_none() {
            self.queue_draw();
        }
    }

    fn fraction_at(&self, x: f64) -> f64 {
        (x / self.width().max(1) as f64).clamp(0.0, 1.0)
    }
}
