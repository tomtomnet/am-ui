//! What stands above the player bar: the sidebar and the pages, Now Playing over them and the
//! queue beside them.
//!
//! The queue is the sidebar of a split view at the end side, so the header bars sort out between
//! themselves which one carries the window's buttons: the queue's while it shows, otherwise the
//! page's (or Now Playing's, which lies inside that split view's content too).

use adw::prelude::*;

use crate::app::ctx;
use crate::widgets::now_playing::NowPlaying;
use crate::widgets::{activity, queue};

/// Put `main` (the sidebar and the pages) under Now Playing and beside the queue. The two views
/// follow `ctx().show_now_playing` and `ctx().show_queue`.
pub fn build(main: &impl IsA<gtk::Widget>) -> gtk::Widget {
    let ctx = ctx();
    let now_playing = NowPlaying::new();
    let covered = gtk::Overlay::new();
    covered.set_child(Some(main));
    covered.add_overlay(&now_playing);
    {
        // what Now Playing covers takes no keyboard focus while it is covered
        let main = main.downgrade();
        ctx.show_now_playing.connect(&now_playing, move |view, shown| {
            if let Some(main) = main.upgrade() {
                main.set_can_focus(!*shown);
            }
            view.set_open(*shown);
        });
    }

    let stage = adw::OverlaySplitView::new();
    stage.set_sidebar_position(gtk::PackType::End);
    stage.set_content(Some(&covered));
    stage.set_sidebar(Some(&queue::build()));
    stage.set_min_sidebar_width(queue::WIDTH);
    stage.set_max_sidebar_width(queue::WIDTH);
    stage.set_show_sidebar(false);
    // swipes belong to the pages and their lists; the queue has its button
    stage.set_enable_show_gesture(false);
    stage.set_enable_hide_gesture(false);
    stage.set_vexpand(true);
    ctx.show_queue.connect(&stage, |stage, shown| show_queue(stage, *shown));
    stage.upcast()
}

/// Show or hide the queue. The split view slides its sidebar in by an animation that divides by
/// the sidebar's width, which it only knows once it has been laid out: on screen but before its
/// first frame (the queue asked for at start-up) the change waits for that frame.
fn show_queue(stage: &adw::OverlaySplitView, shown: bool) {
    if stage.width() > 0 || !stage.is_mapped() {
        stage.set_show_sidebar(shown);
        return;
    }
    stage.add_tick_callback(move |stage, _| {
        activity::note(activity::Source::Queue);
        if stage.width() == 0 {
            return gtk::glib::ControlFlow::Continue;
        }
        stage.set_show_sidebar(shown);
        gtk::glib::ControlFlow::Break
    });
}
