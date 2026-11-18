//! The frequency response of an equalizer preset as a curve: the graph of the Equalizer dialog,
//! the smaller preview of pasted text, and the sparkline of a preset in the list.
//!
//! One widget without children to lay out, drawn in `snapshot()` and only when what it shows
//! changes: a logarithmic axis from 20 Hz to 20 kHz, a dB axis of ±12 dB that grows in steps of
//! 6 dB to fit the curve, a dashed line at 0 dB and the curve, filled toward that line. The curve
//! is in the accent colour, or grey while the equalizer is off; without a preset it is the flat
//! line.

use std::cell::{Cell, RefCell};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk, pango};

use crate::widgets::{text_layout, with_alpha};

/// How much of a graph there is room for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    /// The graph of the dialog: both axes with all their labels.
    #[default]
    Full,
    /// The preview of pasted text: both axes, fewer frequencies labelled.
    Preview,
    /// The curve alone, for a preset in a list.
    Spark,
}

/// The ends of the frequency axis, Hz.
const LOWEST: f32 = 20.0;
const HIGHEST: f32 = 20000.0;
/// The frequencies with a grid line and a label.
const MARKS: [f32; 10] = [20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0, 20000.0];
const FEW_MARKS: [f32; 5] = [20.0, 100.0, 1000.0, 10000.0, 20000.0];
const LABEL_PX: f64 = 11.0;
/// Between the labels of an axis and the plot.
const LABEL_GAP: f32 = 7.0;

/// Half the height of the dB axis: 12 dB, more in steps of 6 dB when the curve reaches further,
/// at most 36 dB (a curve beyond that is cut off there).
fn span(values: &[f32]) -> f32 {
    let peak = values.iter().fold(0.0_f32, |peak, value| peak.max(value.abs()));
    ((peak / 6.0).ceil() * 6.0).clamp(12.0, 36.0)
}

/// The levels with a grid line and a label: every 6 dB, every 12 dB on a tall axis.
fn levels(span: f32) -> Vec<f32> {
    let step = if span <= 24.0 { 6.0 } else { 12.0 };
    let count = (span / step).floor() as i32;
    (-count..=count).map(|index| index as f32 * step).collect()
}

/// "+6", "0", "−12".
fn level_label(db: f32) -> String {
    match db {
        db if db > 0.0 => format!("+{db:.0}"),
        db if db < 0.0 => format!("−{:.0}", -db),
        _ => "0".to_string(),
    }
}

/// "50", "1k", "20k".
fn mark_label(hz: f32) -> String {
    if hz >= 1000.0 { format!("{:.0}k", hz / 1000.0) } else { format!("{hz:.0}") }
}

/// Where a frequency is along the axis, 0..1.
fn along(hz: f32) -> f32 {
    (hz / LOWEST).ln() / (HIGHEST / LOWEST).ln()
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct EqGraph {
        pub kind: Cell<Kind>,
        /// The response in dB at points spaced evenly along the logarithmic axis, 20 Hz first;
        /// fewer than two for no preset.
        pub values: RefCell<Vec<f32>>,
        pub dimmed: Cell<bool>,
        /// Never shown: it has the style class that the style sheet gives the accent colour,
        /// and the curve is drawn in its colour.
        pub accent: adw::Bin,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EqGraph {
        const NAME: &'static str = "AmEqGraph";
        type Type = super::EqGraph;
        type ParentType = gtk::Widget;

        fn class_init(class: &mut Self::Class) {
            class.set_css_name("eqgraph");
            class.set_accessible_role(gtk::AccessibleRole::Img);
        }
    }

    impl ObjectImpl for EqGraph {
        fn constructed(&self) {
            self.parent_constructed();
            self.accent.add_css_class("accent");
            self.accent.set_parent(&*self.obj());
            self.accent.set_child_visible(false);
        }

        fn dispose(&self) {
            self.accent.unparent();
        }
    }

    impl WidgetImpl for EqGraph {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::ConstantSize
        }

        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let horizontal = orientation == gtk::Orientation::Horizontal;
            let (minimum, natural) = match (self.kind.get(), horizontal) {
                (Kind::Full, true) => (280, 560),
                (Kind::Full, false) => (110, 230),
                (Kind::Preview, true) => (200, 250),
                (Kind::Preview, false) => (120, 170),
                (Kind::Spark, true) => (88, 88),
                (Kind::Spark, false) => (36, 36),
            };
            (minimum, natural, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let (width, height) = (obj.width() as f32, obj.height() as f32);
            let text = obj.color();
            let ink = if self.dimmed.get() { with_alpha(&text, 0.45) } else { self.accent.color() };
            let values = self.values.borrow();
            let span = span(&values);
            let (plot, line_width) = match self.kind.get() {
                Kind::Spark => {
                    let plot = graphene::Rect::new(1.0, 1.0, width - 2.0, height - 2.0);
                    let middle = (plot.y() + plot.height() / 2.0).round();
                    snapshot.append_color(
                        &with_alpha(&text, 0.14),
                        &graphene::Rect::new(plot.x(), middle, plot.width(), 1.0),
                    );
                    (plot, 1.5)
                }
                kind => match obj.axes(snapshot, kind, span, &text) {
                    Some(plot) => (plot, 2.0),
                    None => return,
                },
            };
            if plot.width() < 8.0 || plot.height() < 8.0 {
                return;
            }
            let (line, area) = curve(&values, span, &plot);
            if let Some(area) = area {
                // a wash, so that the grid shows through
                snapshot.append_fill(&area, gsk::FillRule::Winding, &with_alpha(&ink, 0.14));
            }
            let stroke = gsk::Stroke::new(line_width);
            stroke.set_line_join(gsk::LineJoin::Round);
            stroke.set_line_cap(gsk::LineCap::Round);
            snapshot.append_stroke(&line, &stroke, &ink);
        }
    }
}

/// The curve through `values` in `plot`, and the area between it and the 0 dB line. Fewer than
/// two values are the flat line, which has no area.
fn curve(values: &[f32], span: f32, plot: &graphene::Rect) -> (gsk::Path, Option<gsk::Path>) {
    let zero = plot.y() + plot.height() / 2.0;
    let (left, right) = (plot.x(), plot.x() + plot.width());
    let line = gsk::PathBuilder::new();
    if values.len() < 2 {
        line.move_to(left, zero);
        line.line_to(right, zero);
        return (line.to_path(), None);
    }
    let area = gsk::PathBuilder::new();
    area.move_to(left, zero);
    for (index, value) in values.iter().enumerate() {
        let x = left + index as f32 / (values.len() - 1) as f32 * plot.width();
        let y = zero - value.clamp(-span, span) / span * (plot.height() / 2.0);
        if index == 0 {
            line.move_to(x, y);
        } else {
            line.line_to(x, y);
        }
        area.line_to(x, y);
    }
    area.line_to(right, zero);
    area.close();
    (line.to_path(), Some(area.to_path()))
}

glib::wrapper! {
    pub struct EqGraph(ObjectSubclass<imp::EqGraph>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl EqGraph {
    pub fn new(kind: Kind) -> EqGraph {
        let graph: EqGraph = glib::Object::new();
        graph.imp().kind.set(kind);
        graph
    }

    /// Show a response: dB at points spaced evenly along the logarithmic axis from 20 Hz to
    /// 20 kHz. Fewer than two points are the flat line of no preset.
    pub fn set_curve(&self, values: &[f32]) {
        let imp = self.imp();
        if imp.values.borrow().as_slice() == values {
            return;
        }
        // a value that is not a number has no place on the axis
        imp.values.replace(values.iter().map(|value| if value.is_finite() { *value } else { 0.0 }).collect());
        self.queue_draw();
    }

    /// Draw the curve grey instead of in the accent colour: the equalizer is off.
    pub fn set_dimmed(&self, dimmed: bool) {
        if self.imp().dimmed.replace(dimmed) != dimmed {
            self.queue_draw();
        }
    }

    /// What the graph shows, for those who cannot see it.
    pub fn set_description(&self, description: &str) {
        self.update_property(&[gtk::accessible::Property::Label(description)]);
    }

    /// Draw the grid and the labels of both axes; returns the area that is left for the curve,
    /// or nothing when there is no room for one.
    fn axes(&self, snapshot: &gtk::Snapshot, kind: Kind, span: f32, text: &gdk::RGBA) -> Option<graphene::Rect> {
        let (width, height) = (self.width() as f32, self.height() as f32);
        let tick = |value: f32, label: String| {
            let layout = text_layout(self, LABEL_PX, pango::Weight::Normal, 0.0);
            layout.set_text(&label);
            let (wide, high) = layout.pixel_size();
            Tick { value, layout, wide: wide as f32, high: high as f32 }
        };
        // the labels that matter most come first: they are placed first
        let mut levels: Vec<Tick> = levels(span).into_iter().map(|db| tick(db, level_label(db))).collect();
        levels.sort_by(|a, b| a.value.abs().total_cmp(&b.value.abs()));
        let marks = if kind == Kind::Full { &MARKS[..] } else { &FEW_MARKS[..] };
        let mut marks: Vec<Tick> = marks.iter().map(|hz| tick(*hz, mark_label(*hz))).collect();
        marks.sort_by_key(|mark| mark_rank(mark.value));
        let line_height = levels.iter().map(|level| level.high).fold(0.0, f32::max);
        let gutter = (levels.iter().map(|level| level.wide).fold(0.0, f32::max) + LABEL_GAP).ceil();
        // the highest and the lowest level are labelled beside the plot's corners, and the
        // highest frequency under its right edge
        let top = (line_height / 2.0).ceil();
        let right = marks.iter().map(|mark| (mark.wide / 2.0).ceil()).fold(0.0, f32::max);
        let plot = graphene::Rect::new(
            gutter,
            top,
            (width - gutter - right).floor(),
            (height - top - line_height - LABEL_GAP).floor(),
        );
        if plot.width() < 8.0 || plot.height() < 8.0 {
            return None;
        }

        let (grid, dim) = (with_alpha(text, 0.09), with_alpha(text, 0.55));
        let put = |tick: &Tick, x: f32, y: f32| {
            snapshot.save();
            snapshot.translate(&graphene::Point::new(x.round(), y.round()));
            snapshot.append_layout(&tick.layout, &dim);
            snapshot.restore();
        };
        // every tick has its line; its label only where it does not run into another
        let mut taken = Vec::new();
        for mark in &marks {
            let x = (plot.x() + along(mark.value) * plot.width()).round();
            snapshot.append_color(&grid, &graphene::Rect::new(x, plot.y(), 1.0, plot.height()));
            let from = (x - mark.wide / 2.0).clamp(0.0, (width - mark.wide).max(0.0));
            if room(&mut taken, from, from + mark.wide, 5.0) {
                put(mark, from, plot.y() + plot.height() + LABEL_GAP);
            }
        }
        let mut taken = Vec::new();
        for level in &levels {
            let y = (plot.y() + (span - level.value) / (2.0 * span) * plot.height()).round();
            if level.value == 0.0 {
                // the line everything is measured from: dashed, and stronger than the grid
                let line = gsk::PathBuilder::new();
                line.move_to(plot.x(), y + 0.5);
                line.line_to(plot.x() + plot.width(), y + 0.5);
                let stroke = gsk::Stroke::new(1.0);
                stroke.set_dash(&[4.0, 4.0]);
                snapshot.append_stroke(&line.to_path(), &stroke, &with_alpha(text, 0.4));
            } else {
                snapshot.append_color(&grid, &graphene::Rect::new(plot.x(), y, plot.width(), 1.0));
            }
            // a line of text is taller than its digits: the lines may meet
            let from = y - level.high / 2.0;
            if room(&mut taken, from, from + level.high, -2.0) {
                put(level, plot.x() - LABEL_GAP - level.wide, from);
            }
        }
        Some(plot)
    }
}

/// A value on an axis and its label.
struct Tick {
    value: f32,
    layout: pango::Layout,
    wide: f32,
    high: f32,
}

/// Which frequency labels are kept when there is no room for all: the decades before the ends
/// of the axis, those before the rest.
fn mark_rank(hz: f32) -> u8 {
    match hz as u32 {
        100 | 1000 | 10000 => 0,
        20 | 20000 => 1,
        _ => 2,
    }
}

/// Whether a label from `from` to `to` along its axis stays `apart` from those in `taken`; it
/// joins them when it does.
fn room(taken: &mut Vec<(f32, f32)>, from: f32, to: f32, apart: f32) -> bool {
    let free = taken.iter().all(|(start, end)| to + apart <= *start || *end + apart <= from);
    if free {
        taken.push((from, to));
    }
    free
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_db_axis_grows_in_steps_of_six() {
        assert_eq!(span(&[]), 12.0);
        assert_eq!(span(&[0.0, 3.0, -11.9]), 12.0);
        assert_eq!(span(&[12.0, -12.0]), 12.0);
        assert_eq!(span(&[-12.1]), 18.0);
        assert_eq!(span(&[4.0, -15.2]), 18.0);
        assert_eq!(span(&[23.0]), 24.0);
        assert_eq!(span(&[-31.0]), 36.0);
        assert_eq!(span(&[-120.0]), 36.0);
    }

    #[test]
    fn the_axes_are_labelled_at_round_values() {
        assert_eq!(levels(12.0), [-12.0, -6.0, 0.0, 6.0, 12.0]);
        assert_eq!(levels(18.0).len(), 7);
        assert_eq!(levels(24.0).len(), 9);
        assert_eq!(levels(30.0), [-24.0, -12.0, 0.0, 12.0, 24.0]);
        assert_eq!(levels(36.0), [-36.0, -24.0, -12.0, 0.0, 12.0, 24.0, 36.0]);
        let labels: Vec<String> = levels(12.0).into_iter().map(level_label).collect();
        assert_eq!(labels, ["−12", "−6", "0", "+6", "+12"]);
        let labels: Vec<String> = MARKS.into_iter().map(mark_label).collect();
        assert_eq!(labels, ["20", "50", "100", "200", "500", "1k", "2k", "5k", "10k", "20k"]);
    }

    #[test]
    fn the_frequency_axis_is_logarithmic() {
        assert_eq!(along(20.0), 0.0);
        assert!((along(20000.0) - 1.0).abs() < 1e-6);
        // a decade is a third of the axis
        assert!((along(200.0) - 1.0 / 3.0).abs() < 1e-6);
        assert!((along(2000.0) - 2.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn labels_that_would_touch_are_left_out() {
        let mut taken = Vec::new();
        assert!(room(&mut taken, 100.0, 120.0, 5.0));
        // too close on either side, or over it
        assert!(!room(&mut taken, 122.0, 140.0, 5.0));
        assert!(!room(&mut taken, 80.0, 96.0, 5.0));
        assert!(!room(&mut taken, 105.0, 110.0, 5.0));
        // far enough
        assert!(room(&mut taken, 125.0, 140.0, 5.0));
        assert!(room(&mut taken, 70.0, 95.0, 5.0));
        assert_eq!(taken.len(), 3);
        // lines of text may meet by two pixels
        let mut taken = vec![(10.0, 25.0)];
        assert!(room(&mut taken, 23.0, 38.0, -2.0));
        assert!(!room(&mut taken, 35.0, 50.0, -2.0));

        // the decades are placed before the ends of the axis, those before the rest
        let mut marks = MARKS.to_vec();
        marks.sort_by_key(|hz| mark_rank(*hz));
        assert_eq!(marks, [100.0, 1000.0, 10000.0, 20.0, 20000.0, 50.0, 200.0, 500.0, 2000.0, 5000.0]);
    }
}
