//! Draw a window into a PNG file without a screen: for checking the user interface headlessly.

use std::path::PathBuf;
use std::time::Duration;

use adw::prelude::*;
use gtk::{glib, graphene, gsk};

pub fn save(window: &adw::ApplicationWindow, path: &std::path::Path) -> Result<(), String> {
    let (width, height) = (window.width() as f32, window.height() as f32);
    // device pixels, as the display would show it
    let scale = window.surface().map(|surface| surface.scale() as f32).unwrap_or(1.0);
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let snapshot = gtk::Snapshot::new();
    snapshot.scale(scale, scale);
    paintable.snapshot(&snapshot, width as f64, height as f64);
    // a popover is a surface of its own, which the window's picture does not include: the open
    // ones are drawn where they are
    let mut popovers = Vec::new();
    open_popovers(window.upcast_ref(), &mut popovers);
    let (window_x, window_y) = window.surface_transform();
    for popover in popovers {
        let Some(popup) = popover.surface().and_downcast::<gtk::gdk::Popup>() else { continue };
        let (own_x, own_y) = popover.surface_transform();
        let x = popup.position_x() as f64 + own_x - window_x;
        let y = popup.position_y() as f64 + own_y - window_y;
        snapshot.save();
        snapshot.translate(&graphene::Point::new(x as f32, y as f32));
        gtk::WidgetPaintable::new(Some(&popover)).snapshot(&snapshot, popover.width() as f64, popover.height() as f64);
        snapshot.restore();
    }
    let node = snapshot.to_node().ok_or("the window has nothing to draw yet")?;
    let renderer = window.native().and_then(|native| native.renderer()).ok_or("the window has no renderer")?;
    let bounds = graphene::Rect::new(0.0, 0.0, width * scale, height * scale);
    let texture = gsk::prelude::GskRendererExt::render_texture(&renderer, &node, Some(&bounds));
    texture.save_to_png(path).map_err(|error| error.to_string())
}

/// The popovers that are open under `widget` (menus, the sound settings).
fn open_popovers(widget: &gtk::Widget, found: &mut Vec<gtk::Popover>) {
    let mut child = widget.first_child();
    while let Some(widget) = child {
        if let Some(popover) = widget.downcast_ref::<gtk::Popover>()
            && popover.is_mapped()
        {
            found.push(popover.clone());
        }
        open_popovers(&widget, found);
        child = widget.next_sibling();
    }
}

/// Save the window once it has been on screen for a moment, then quit the application.
pub fn when_settled(window: &adw::ApplicationWindow, path: PathBuf) {
    let window = window.clone();
    // AMUI_SHOT_DELAY_MS: how long to wait for the content (a busy machine needs longer)
    let delay = std::env::var("AMUI_SHOT_DELAY_MS").ok().and_then(|value| value.parse().ok()).unwrap_or(1200);
    glib::timeout_add_local_once(Duration::from_millis(delay), move || {
        match save(&window, &path) {
            Ok(()) => println!("saved {}", path.display()),
            Err(error) => eprintln!("am-ui: screenshot failed: {error}"),
        }
        if let Some(application) = window.application() {
            application.quit();
        }
    });
}
