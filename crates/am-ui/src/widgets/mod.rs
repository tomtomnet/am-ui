//! The application's own widgets.

pub mod activity;
pub mod card;
pub mod columns;
pub mod cover;
pub mod embedded;
pub mod eq_graph;
pub mod equalizer;
pub mod excerpt;
pub mod glow;
pub mod hero;
pub mod item_row;
pub mod lyrics;
pub mod now_playing;
pub mod player_bar;
pub mod playlist_list;
pub mod queue;
pub mod seek_bar;
pub mod shelf;
pub mod signin;
pub mod song_cell;
pub mod sound_menu;
pub mod stage;
pub mod thumb;
pub mod track_row;

use gtk::prelude::*;
use gtk::{gdk, pango};

/// A colour with its opacity multiplied by `alpha`.
pub fn with_alpha(colour: &gdk::RGBA, alpha: f32) -> gdk::RGBA {
    gdk::RGBA::new(colour.red(), colour.green(), colour.blue(), colour.alpha() * alpha)
}

/// The size of the usual text in logical pixels (11 points) when the desktop does not make text
/// larger or smaller.
const USUAL_PX: f64 = 11.0 * 96.0 / 72.0;

/// `size` (Pango units) and `weight` only: the family is the widget's own, also when its style
/// changes later.
fn font(size: f64, weight: pango::Weight) -> pango::FontDescription {
    let mut font = pango::FontDescription::new();
    font.set_absolute_size(size);
    font.set_weight(weight);
    font
}

/// The size of the widget's font in Pango units, as its style gives it: it follows the
/// desktop's text size and font.
fn font_size(widget: &impl IsA<gtk::Widget>) -> f64 {
    match widget.pango_context().font_description() {
        Some(font) if font.size() > 0 && font.is_size_absolute() => font.size() as f64,
        // points, at the 96 dots per inch of logical pixels
        Some(font) if font.size() > 0 => font.size() as f64 * 96.0 / 72.0,
        _ => USUAL_PX * pango::SCALE as f64,
    }
}

fn one_line(layout: &pango::Layout, width: f32) {
    layout.set_single_paragraph_mode(true);
    if width > 0.0 {
        layout.set_width((width * pango::SCALE as f32) as i32);
        layout.set_ellipsize(pango::EllipsizeMode::End);
    }
}

/// A one-line text layout in the widget's font at `px` logical pixels whatever the desktop's
/// text size, cut with an ellipsis at `width` when that is positive: for text that belongs to a
/// picture (the initials on an artist's tile, the scale of a graph).
pub fn text_layout(widget: &impl IsA<gtk::Widget>, px: f64, weight: pango::Weight, width: f32) -> pango::Layout {
    let layout = widget.create_pango_layout(None);
    layout.set_font_description(Some(&font(px * pango::SCALE as f64, weight)));
    one_line(&layout, width);
    layout
}

/// The font of `font_layout`: `em` times the size of the widget's font.
pub fn relative_font(widget: &impl IsA<gtk::Widget>, em: f64, weight: pango::Weight) -> pango::FontDescription {
    font(font_size(widget) * em, weight)
}

/// A one-line text layout at `em` times the size of the widget's font, cut with an ellipsis at
/// `width` when that is positive: text that grows with the desktop's text size, as a label's
/// does. A layout that is kept takes `relative_font` again when the font changes
/// (`font_serial`).
pub fn font_layout(widget: &impl IsA<gtk::Widget>, em: f64, weight: pango::Weight, width: f32) -> pango::Layout {
    let layout = widget.create_pango_layout(None);
    layout.set_font_description(Some(&relative_font(widget, em, weight)));
    one_line(&layout, width);
    layout
}

/// The height of one line of text at `em` times the size of the widget's font.
pub fn line_height(widget: &impl IsA<gtk::Widget>, em: f64, weight: pango::Weight) -> i32 {
    let layout = widget.create_pango_layout(Some("Ag"));
    layout.set_font_description(Some(&relative_font(widget, em, weight)));
    layout.pixel_size().1
}

/// The height of one line of `label`, whatever text it shows: its font as its style gives it
/// (family, size, weight). A row that is as tall as its lines need measures them this way, so
/// that every row of a list has the same height and all of them grow with the desktop's text
/// size.
pub fn label_line(label: &gtk::Label) -> i32 {
    label.create_pango_layout(Some("Ag")).pixel_size().1
}

/// The width of `text` in `label`'s font and with its attributes (the tabular figures of the
/// "numeric" style class): what a column needs for the widest text it shows ("00:00").
pub fn label_width(label: &gtk::Label, text: &str) -> i32 {
    let layout = label.create_pango_layout(Some(text));
    layout.set_attributes(label.layout().attributes().as_ref());
    layout.pixel_size().0
}

/// The desktop's text scaling factor (1 when text has its usual size): for the widths that are
/// chosen to hold some text (a card, a column of songs), which hold as much of it when they grow
/// with this. GTK measures and draws a widget again when it changes.
pub fn text_scaling(widget: &impl IsA<gtk::Widget>) -> f64 {
    let dpi = widget.settings().gtk_xft_dpi();
    if dpi > 0 { (dpi as f64 / (96.0 * 1024.0)).clamp(0.5, 4.0) } else { 1.0 }
}

/// `px` (a width at the usual text size) at the desktop's text scaling factor.
pub fn scaled(widget: &impl IsA<gtk::Widget>, px: i32) -> i32 {
    (px as f64 * text_scaling(widget)).round() as i32
}

/// A number that changes whenever the font of one of `widgets` does: the desktop's text size or
/// font, or their style. What was measured in those fonts is measured again then.
pub fn font_serial(widgets: &[&gtk::Widget]) -> u64 {
    widgets.iter().fold(0, |serial: u64, widget| {
        serial.wrapping_mul(0x0000_0100_0000_01b3).wrapping_add(u64::from(widget.pango_context().serial()))
    })
}

/// Make `label` a link (the artist or the album of a song in its row): while it is one
/// (`set_link`), the pointer is a hand over it and underlines it (the style class "text-link"),
/// and a click on it runs `clicked` instead of what a click does to the widgets around it (the
/// row's own click, the selection of a list). All of the label is the link: give it no more
/// width than its text.
pub fn make_link(label: &gtk::Label, clicked: impl Fn() + 'static) {
    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    click.connect_pressed(|gesture, _, _, _| {
        if gesture.widget().is_some_and(|label| label.has_css_class("text-link")) {
            gesture.set_state(gtk::EventSequenceState::Claimed);
        }
    });
    click.connect_released(move |gesture, _, x, y| {
        // released where it was pressed: a press that slid off the link does nothing
        if gesture.widget().is_some_and(|label| label.has_css_class("text-link") && label.contains(x, y)) {
            clicked();
        }
    });
    label.add_controller(click);
}

/// Whether `label` (made a link by `make_link`) is one now: there is a page it leads to.
pub fn set_link(label: &gtk::Label, linked: bool) {
    if linked == label.has_css_class("text-link") {
        return;
    }
    if linked {
        label.add_css_class("text-link");
        label.set_cursor_from_name(Some("pointer"));
    } else {
        label.remove_css_class("text-link");
        label.set_cursor(None::<&gdk::Cursor>);
    }
}

/// "3:07", "1:02:45".
pub fn clock(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as i64;
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 { format!("{hours}:{minutes:02}:{seconds:02}") } else { format!("{minutes}:{seconds:02}") }
}

/// "13 songs, 1 hour 14 minutes".
pub fn total_time(count: usize, seconds: f64) -> String {
    let minutes = (seconds / 60.0).round() as i64;
    let songs = if count == 1 { "1 song".to_string() } else { format!("{count} songs") };
    let unit = |value: i64, name: &str| if value == 1 { format!("1 {name}") } else { format!("{value} {name}s") };
    let time = match (minutes / 60, minutes % 60) {
        (0, minutes) => unit(minutes, "minute"),
        (hours, 0) => unit(hours, "hour"),
        (hours, minutes) => format!("{} {}", unit(hours, "hour"), unit(minutes, "minute")),
    };
    format!("{songs}, {time}")
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use gtk::glib;

    /// A window on the display with a widget in it, for the checks that need one.
    pub(crate) struct Stage {
        pub window: gtk::Window,
        drawn: Rc<Cell<u32>>,
    }

    impl Stage {
        pub(crate) fn new(child: &impl IsA<gtk::Widget>, width: i32, height: i32) -> Stage {
            let window = gtk::Window::new();
            window.set_default_size(width, height);
            window.set_child(Some(child));
            window.present();
            let drawn = Rc::new(Cell::new(0));
            let counted = drawn.clone();
            window.add_tick_callback(move |_, _| {
                counted.set(counted.get() + 1);
                glib::ControlFlow::Continue
            });
            let stage = Stage { window, drawn };
            stage.frames(20);
            stage
        }

        /// Let the window draw `count` frames.
        pub(crate) fn frames(&self, count: u32) {
            let until = self.drawn.get() + count;
            let context = glib::MainContext::default();
            while self.drawn.get() < until {
                context.iteration(true);
            }
        }
    }

    /// What can only be checked with a window on a display, in one test because GTK lives on
    /// one thread: `crates/am-ui/tools/headless.sh cargo test -p am-ui -- --ignored display`.
    #[test]
    #[ignore = "needs a display (run it under tools/headless.sh)"]
    fn widgets_on_a_display() {
        // only in the private compositor of tools/headless.sh, never on the desktop's display
        if !std::env::var("WAYLAND_DISPLAY").is_ok_and(|name| name.starts_with("wl-")) {
            eprintln!("widgets_on_a_display was not run: start it through tools/headless.sh");
            return;
        }
        gtk::init().unwrap();
        embedded::tests::on_a_display();
        hero::tests::on_a_display();
        track_row::tests::on_a_display();
        equalizer::tests::on_a_display();
    }

    /// Every label of `container` (among its own children) that shows something is as tall as
    /// its text and lies inside the container: no text is cut off at its top or bottom.
    pub(crate) fn assert_text_fits(container: &gtk::Widget, what: &str) {
        let height = container.height() as f32;
        let mut child = container.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            let Some(label) = widget.downcast_ref::<gtk::Label>() else { continue };
            if !label.is_visible() || !label.is_child_visible() || label.text().is_empty() {
                continue;
            }
            let (_, natural, _, _) = label.measure(gtk::Orientation::Vertical, -1);
            let bounds = label.compute_bounds(container).expect("a child of the container");
            let (top, bottom) = (bounds.y(), bounds.y() + bounds.height());
            let text = label.text();
            assert!(bounds.height() >= natural as f32, "{what}: “{text}” is {} tall for {natural}", bounds.height());
            assert!(top >= 0.0 && bottom <= height, "{what}: “{text}” at {top}..{bottom} is outside its {height}");
        }
    }

    #[test]
    fn times() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(187.4), "3:07");
        assert_eq!(clock(3765.0), "1:02:45");
        assert_eq!(total_time(1, 59.0), "1 song, 1 minute");
        assert_eq!(total_time(13, 4440.0), "13 songs, 1 hour 14 minutes");
        assert_eq!(total_time(500, 7200.0), "500 songs, 2 hours");
    }
}
