//! am-ui: a native Apple Music client for the GNOME desktop (GTK 4 + libadwaita) on the `amui`
//! engine.
//!
//!   am-ui                        the player
//!   am-ui --demo                 the user interface with synthetic data, no account
//!   am-ui --page NAME            start on a page (home, new, radio, recent, artists, albums, songs, playlists)
//!   am-ui --open KIND:ID         open an album, a playlist or an artist (album:l.3, playlist:p.0, artist:NAME or ID)
//!   am-ui --scroll PIXELS        scroll that page down (PIXELS or end; for screenshots)
//!   am-ui --sort KEY             the order of the library's pages (title, artist, album, recently-added, release-date)
//!   am-ui --view grid|list       how the library's albums, artists and playlists are shown
//!   am-ui --screenshot OUT.png   draw the window into a PNG file once it has loaded, then quit
//!   am-ui --size WxH             window size (for screenshots)
//!   am-ui --text-scale FACTOR    text FACTOR times its size, as the desktop's text scaling factor makes it (1.25, 2...)
//!   am-ui --playing              start with a song playing (demonstration)
//!   am-ui --song N               which song of the demonstration's queue that is (0 to 14: their lyrics differ)
//!   am-ui --seek SECONDS         start that song at a position
//!   am-ui --paused               pause that song as soon as it plays
//!   am-ui --now-playing, --queue start with those views open
//!   am-ui --show NAME            start with a dialog open (preferences, shortcuts, about, new-playlist,
//!                                sign-in, sign-in-code, sound, equalizer, equalizer-add,
//!                                equalizer-menu, equalizer-drop, view-options...)
//!   am-ui --signed-out           the demonstration without a sign-in
//!   am-ui --search TERM          start with TERM in the search field and what it finds on the Search page
//!   am-ui --search-scope NAME    which results that page shows first (apple-music, library)
//!   am-ui --at SECONDS ACTION    activate an action of the window by itself (win.play-pause, win.now-playing,
//!                                win.queue, win.next, win.go-to-song, app.quit...): for checks without a screen
//!   am-ui --activity             print once a second how many frames the window drew

mod account;
mod actions;
mod app;
mod art;
mod data;
mod demo;
mod demo_playback;
mod dialogs;
mod diff;
mod edits;
mod engine;
mod equalizer;
mod favorites;
mod finder;
mod library;
mod links;
mod menus;
mod mpris;
mod nav;
mod pages;
mod playback;
mod player;
mod prefs;
mod regions;
mod scroll;
mod settings;
mod shortcuts;
mod shot;
mod sidebar;
mod signal;
mod sorting;
mod widgets;
mod window;

use adw::prelude::*;
use gtk::{gio, glib};

pub const APP_ID: &str = "dev.amui.Music";

/// A Vulkan driver of a graphics card that is installed but not driven by its own driver (one
/// handed to a virtual machine, for example) stalls every Vulkan start for seconds. GTK renders
/// with Vulkan: hide that driver from it.
fn hide_idle_vulkan_driver() {
    if std::env::var_os("VK_LOADER_DRIVERS_DISABLE").is_some() {
        return;
    }
    let Ok(devices) = std::fs::read_dir("/sys/bus/pci/devices") else { return };
    for device in devices.flatten() {
        let path = device.path();
        let read = |name: &str| std::fs::read_to_string(path.join(name)).unwrap_or_default();
        if read("vendor").trim() != "0x10de" || !read("class").trim().starts_with("0x03") {
            continue;
        }
        let driver = std::fs::read_link(path.join("driver"))
            .ok()
            .and_then(|link| link.file_name().map(|name| name.to_string_lossy().into_owned()))
            .unwrap_or_default();
        if driver != "nvidia" && driver != "nouveau" {
            // before any thread exists: nothing else reads the environment yet
            unsafe { std::env::set_var("VK_LOADER_DRIVERS_DISABLE", "*nvidia*") };
            return;
        }
    }
}

fn main() -> glib::ExitCode {
    let args: Vec<String> = std::env::args_os().skip(1).map(|word| word.to_string_lossy().into_owned()).collect();
    // `am-ui --wrapper ARGS`: this program as the lossless route's control helper, which the
    // engine starts for the sign-in and the lossless service; nothing of the window is made
    if args.first().is_some_and(|word| word == "--wrapper") {
        std::process::exit(amui::wrapper::main("am-ui --wrapper", &args[1..]));
    }
    let options = match app::Options::parse(args.into_iter()) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("am-ui: {message}");
            return glib::ExitCode::from(2);
        }
    };
    // what starts a song by itself is for the demonstration only: a check without a screen
    // must never play through the real output
    let plays = options.playing || options.song.is_some() || options.seek.is_some() || options.paused;
    if !options.demo && (plays || !options.script.is_empty()) {
        eprintln!("am-ui: --playing, --song, --seek, --paused and --at work with --demo only");
        return glib::ExitCode::from(2);
    }
    hide_idle_vulkan_driver();
    gio::resources_register_include!("am-ui.gresource").expect("the program carries its resources");
    // a demonstration or a screenshot never talks to a running player
    let standalone = options.demo || options.screenshot.is_some();
    let application = adw::Application::builder()
        .application_id(if standalone { format!("{APP_ID}.Demo") } else { APP_ID.to_string() })
        .flags(if standalone { gio::ApplicationFlags::NON_UNIQUE } else { gio::ApplicationFlags::empty() })
        .build();
    application.connect_activate(move |application| app::activate(application, &options));
    application.connect_shutdown(|_| app::shut_down());
    application.run_with_args::<&str>(&[])
}
