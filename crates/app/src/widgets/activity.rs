//! Counts what moves by itself in Now Playing, the lyrics and the queue.
//!
//! Every tick callback and every timer of those views calls `note()`. `am-ui --activity` prints
//! once a second how many of them ran and how many frames the window produced, so that a run
//! without a screen can show that nothing does while a view is hidden or playback is paused (an
//! idle window draws nothing).

use std::cell::Cell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

/// Whose callback ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The glow: the timer that drifts its blobs and changes its colours.
    Glow,
    /// The lyrics: the tick callback of a glide or a word being filled, the timer that waits
    /// for the next line, the one that resumes following after the user scrolled.
    Lyrics,
    /// Now Playing itself: sliding in and out, the artwork block changing its place, the timers
    /// for the pointer, the spinner and the artwork.
    View,
    /// The queue: scrolling while a row is dragged at its edge.
    Queue,
}

const SOURCES: [(Source, &str); 4] =
    [(Source::Glow, "glow"), (Source::Lyrics, "lyrics"), (Source::View, "view"), (Source::Queue, "queue")];

thread_local! {
    static CALLBACKS: [Cell<u64>; 4] = const { [Cell::new(0), Cell::new(0), Cell::new(0), Cell::new(0)] };
}

/// A tick callback or a timer of one of these views ran.
pub fn note(source: Source) {
    CALLBACKS.with(|counts| {
        let count = &counts[source as usize];
        count.set(count.get() + 1);
    });
}

fn callbacks() -> [u64; 4] {
    CALLBACKS.with(|counts| [counts[0].get(), counts[1].get(), counts[2].get(), counts[3].get()])
}

/// Print once a second what the window of `widget` did in that second: the frames it drew, the
/// frames in which a tick callback of anything in it ran, and the callbacks counted by `note()`.
pub fn report(widget: &impl IsA<gtk::Widget>) {
    let Some(clock) = widget.frame_clock() else { return };
    let (drawn, ticked) = (Rc::new(Cell::new(0u32)), Rc::new(Cell::new(0u32)));
    {
        let drawn = drawn.clone();
        clock.connect_paint(move |_| drawn.set(drawn.get() + 1));
        let ticked = ticked.clone();
        clock.connect_update(move |_| ticked.set(ticked.get() + 1));
    }
    let (mut second, mut before) = (0u32, callbacks());
    glib::timeout_add_seconds_local(1, move || {
        second += 1;
        let now = callbacks();
        let own: Vec<String> = SOURCES
            .iter()
            .map(|(source, name)| format!("{name} {}", now[*source as usize] - before[*source as usize]))
            .collect();
        println!(
            "activity {second:>2} s: {:>3} frames drawn, {:>3} ticked; callbacks: {}",
            drawn.replace(0),
            ticked.replace(0),
            own.join(", ")
        );
        before = now;
        glib::ControlFlow::Continue
    });
}
