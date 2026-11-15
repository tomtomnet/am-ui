//! Start-up: command-line options and what the widgets of the window share.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use amui::config::Config;

use crate::account::{Account, AccountService, AccountState, DemoAccount};
use crate::art::ArtLoader;
use crate::data::Music;
use crate::demo::DemoMusic;
use crate::demo_playback::DemoPlayback;
use crate::equalizer::EqualizerService;
use crate::favorites::Favorites;
use crate::library::Library;
use crate::nav::{Dest, Navigator};
use crate::pages::detail::Target;
use crate::pages::search::{Scope, Search};
use crate::playback::Playback;
use crate::player::Player;
use crate::settings::Settings;
use crate::signal::Signal;
use crate::{shot, window};

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Synthetic data, no account, no audio.
    pub demo: bool,
    /// The page shown first.
    pub page: Option<Dest>,
    /// Draw the window into this PNG file once it has loaded, then quit.
    pub screenshot: Option<PathBuf>,
    /// Window size in logical pixels.
    pub size: Option<(i32, i32)>,
    /// Start with a song playing (demonstration).
    pub playing: bool,
    /// Start with the Now Playing view open.
    pub now_playing: bool,
    /// Start with the queue shown.
    pub queue: bool,
    /// Start with a dialog open: preferences, shortcuts, about, new-playlist, sign-in.
    /// Also `equalizer` and its states (see `widgets::equalizer::show_scene`).
    pub show: Option<String>,
    /// Start with this term in the search field and what it finds on the Search page.
    pub search: Option<String>,
    /// Which results the Search page shows first: Apple Music's or the library's.
    pub search_scope: Option<Scope>,
    /// Start signed out (demonstration).
    pub signed_out: bool,
    /// Open the page of an album, a playlist or an artist.
    pub open: Option<Target>,
    /// Scroll that page down by so many pixels (for screenshots).
    pub scroll: Option<f64>,
    /// The order of the library's pages that offer it, instead of the one chosen last.
    pub sort: Option<crate::sorting::Order>,
    /// A grid or a list for the library's pages that offer both.
    pub view: Option<crate::pages::view_options::View>,
    /// Which song of the demonstration's queue `playing` starts (they differ in their lyrics).
    pub song: Option<usize>,
    /// Start that song this many seconds in.
    pub seek: Option<f64>,
    /// Pause that song as soon as it plays.
    pub paused: bool,
    /// Actions of the window to activate by themselves, so many seconds after the start: what a
    /// check without a screen does instead of clicking.
    pub script: Vec<(f64, String)>,
    /// Print once a second how many frames the window drew.
    pub activity: bool,
    /// Draw text this many times its size, as the desktop's text scaling factor does (for checks
    /// of larger text).
    pub text_scale: Option<f64>,
}

/// A number of seconds given to the option `name`.
fn seconds(name: &str, text: &str) -> Result<f64, String> {
    let wanted = text.parse().ok().filter(|seconds: &f64| seconds.is_finite() && *seconds >= 0.0);
    wanted.ok_or_else(|| format!("{name} wants a number of seconds, not {text}"))
}

impl Options {
    pub fn parse(args: impl Iterator<Item = String>) -> Result<Options, String> {
        let mut options = Options::default();
        let mut args = args;
        while let Some(arg) = args.next() {
            let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
            match arg.as_str() {
                "--demo" => options.demo = true,
                "--playing" => options.playing = true,
                "--now-playing" => options.now_playing = true,
                "--queue" => options.queue = true,
                "--signed-out" => options.signed_out = true,
                "--show" => options.show = Some(value("--show")?),
                "--search" => options.search = Some(value("--search")?),
                "--search-scope" => {
                    let name = value("--search-scope")?;
                    options.search_scope =
                        Some(Scope::parse(&name).ok_or_else(|| format!("unknown search scope {name}"))?);
                }
                "--open" => options.open = Some(Target::parse(&value("--open")?)?),
                "--scroll" => {
                    let text = value("--scroll")?;
                    let offset =
                        if text == "end" { Some(f64::MAX) } else { text.parse().ok().filter(|n: &f64| *n >= 0.0) };
                    options.scroll = Some(offset.ok_or_else(|| format!("--scroll wants pixels or end, not {text}"))?);
                }
                "--sort" => {
                    let name = value("--sort")?;
                    let order = crate::sorting::Order::parse(&name);
                    options.sort = Some(order.ok_or_else(|| format!("unknown order {name}"))?);
                }
                "--view" => {
                    let name = value("--view")?;
                    let view = crate::pages::view_options::View::parse(&name);
                    options.view = Some(view.ok_or_else(|| format!("--view wants grid or list, not {name}"))?);
                }
                "--paused" => options.paused = true,
                "--activity" => options.activity = true,
                "--text-scale" => {
                    let text = value("--text-scale")?;
                    let scale = text.parse().ok().filter(|scale: &f64| (0.5..=4.0).contains(scale));
                    options.text_scale =
                        Some(scale.ok_or_else(|| format!("--text-scale wants a factor from 0.5 to 4, not {text}"))?);
                }
                "--song" => {
                    let text = value("--song")?;
                    options.song = Some(text.parse().map_err(|_| format!("--song wants a number, not {text}"))?);
                }
                "--seek" => options.seek = Some(seconds("--seek", &value("--seek")?)?),
                "--at" => {
                    let after = seconds("--at", &value("--at")?)?;
                    options.script.push((after, value("--at")?));
                }
                "--page" => {
                    let name = value("--page")?;
                    options.page = Some(Dest::parse(&name).ok_or_else(|| format!("unknown page {name}"))?);
                }
                "--screenshot" => options.screenshot = Some(PathBuf::from(value("--screenshot")?)),
                "--size" => {
                    let text = value("--size")?;
                    let size = text.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
                    options.size = Some(size.ok_or_else(|| format!("--size wants WIDTHxHEIGHT, not {text}"))?);
                }
                other => return Err(format!("unknown option {other}")),
            }
        }
        Ok(options)
    }
}

/// Everything the widgets of the window share.
pub struct Ctx {
    pub options: Options,
    pub music: Arc<dyn Music>,
    pub library: Rc<Library>,
    pub favorites: Rc<Favorites>,
    pub nav: Rc<Navigator>,
    pub search: Rc<Search>,
    pub player: Rc<Player>,
    /// The equalizer (its presets, on or off) for the Equalizer dialog; called on worker threads.
    pub equalizer: Arc<dyn crate::equalizer::EqualizerService>,
    /// A short message for the user.
    pub toast: Signal<String>,
    /// Show or hide the Now Playing view.
    pub show_now_playing: Signal<bool>,
    /// Show or hide the queue.
    pub show_queue: Signal<bool>,
    /// The songs or the details of this library playlist changed.
    pub playlist_changed: Signal<String>,
    /// This library playlist was deleted.
    pub playlist_deleted: Signal<String>,
    /// The user asked for fresh data: pages fetch what they show again.
    pub refreshed: Signal<()>,
    pub settings: Rc<Settings>,
    pub account: Rc<Account>,
}

thread_local! {
    static CTX: RefCell<Option<Rc<Ctx>>> = const { RefCell::new(None) };
}

/// The shared state of the application (user interface thread only).
pub fn ctx() -> Rc<Ctx> {
    CTX.with(|slot| slot.borrow().clone().expect("the application state exists once the window is built"))
}

fn load_style() {
    let Some(display) = gtk::gdk::Display::default() else { return };
    // the application's own icons (resources/icons, packed into the program)
    gtk::IconTheme::for_display(&display).add_resource_path("/dev/amui/Music/icons");
    let style = gtk::CssProvider::new();
    style.load_from_string(include_str!("style.css"));
    gtk::style_context_add_provider_for_display(&display, &style, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    // the dark palette comes and goes with the desktop's appearance setting
    let dark = gtk::CssProvider::new();
    dark.load_from_string(include_str!("style-dark.css"));
    let manager = adw::StyleManager::default();
    let apply = move |manager: &adw::StyleManager| {
        if manager.is_dark() {
            gtk::style_context_add_provider_for_display(&display, &dark, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
        } else {
            gtk::style_context_remove_provider_for_display(&display, &dark);
        }
    };
    apply(&manager);
    manager.connect_dark_notify(apply);
}

fn install_app_actions(application: &adw::Application, window: &adw::ApplicationWindow) {
    let weak = window.downgrade();
    let preferences = gtk::gio::SimpleAction::new("preferences", None);
    preferences.connect_activate(move |_, _| {
        if let Some(window) = weak.upgrade() {
            crate::prefs::show_preferences(&window);
        }
    });
    application.add_action(&preferences);
    let weak = window.downgrade();
    let about = gtk::gio::SimpleAction::new("about", None);
    about.connect_activate(move |_, _| {
        if let Some(window) = weak.upgrade() {
            crate::prefs::show_about(&window);
        }
    });
    application.add_action(&about);
    let weak = window.downgrade();
    let sign_in = gtk::gio::SimpleAction::new("sign-in", None);
    sign_in.connect_activate(move |_, _| {
        if let Some(window) = weak.upgrade() {
            crate::widgets::signin::show(&window);
        }
    });
    application.add_action(&sign_in);
    let weak = window.downgrade();
    let equalizer = gtk::gio::SimpleAction::new("equalizer", None);
    equalizer.connect_activate(move |_, _| {
        if let Some(window) = weak.upgrade() {
            crate::widgets::equalizer::show(&window);
        }
    });
    application.add_action(&equalizer);
}

/// Bring back what was saved (volume, repeat, shuffle, the Atmos switch, the window's size) and
/// save those again whenever they change.
fn remember(ctx: &Rc<Ctx>, window: &adw::ApplicationWindow) {
    use amui::types::{AudioMode, RepeatMode};
    let (settings, playback) = (ctx.settings.clone(), ctx.player.playback.clone());
    playback.set_volume(settings.number("volume", 1.0).clamp(0.0, 1.0));
    if let Some(mode) = AudioMode::parse(&settings.text("mode", "atmos")) {
        playback.set_mode(mode);
    }
    if let Some(repeat) = RepeatMode::parse(&settings.text("repeat", "off")) {
        playback.set_repeat(repeat);
    }
    playback.set_shuffle(settings.bool("shuffle", false));
    playback.set_stereo_fallback(settings.bool("stereoFallback", true));
    playback.set_autoplay(settings.bool("autoplay", true));
    let weak = Rc::downgrade(ctx);
    ctx.player.changed.connect_forever(move |_| {
        let Some(ctx) = weak.upgrade() else { return };
        let view = ctx.player.view();
        ctx.settings.set("volume", view.volume);
        ctx.settings.set("mode", view.mode.as_str());
        ctx.settings.set("repeat", view.repeat.as_str());
        ctx.settings.set("shuffle", view.shuffle);
        ctx.settings.set("stereoFallback", view.stereo_fallback);
        ctx.settings.set("autoplay", view.autoplay);
    });
    // the queue stands beside the pages again when it did last time
    if settings.bool("queueShown", false) {
        ctx.show_queue.emit(&true);
    }
    let weak = Rc::downgrade(ctx);
    ctx.show_queue.connect_forever(move |shown| {
        if let Some(ctx) = weak.upgrade() {
            ctx.settings.set("queueShown", *shown);
        }
    });
    if ctx.options.size.is_none() {
        let width = settings.number("windowWidth", 1280.0) as i32;
        let height = settings.number("windowHeight", 820.0) as i32;
        window.set_default_size(width.max(720), height.max(480));
        if settings.bool("windowMaximized", false) {
            window.maximize();
        }
    }
    let weak = Rc::downgrade(ctx);
    window.connect_close_request(move |window| {
        if let Some(ctx) = weak.upgrade() {
            if !window.is_maximized() {
                ctx.settings.set("windowWidth", window.width());
                ctx.settings.set("windowHeight", window.height());
            }
            ctx.settings.set("windowMaximized", window.is_maximized());
            ctx.settings.save();
            ctx.player.playback.shutdown();
        }
        gtk::glib::Propagation::Proceed
    });
}

/// The music service and the account on the engine: Apple's service with this machine's sign-in.
type Services = (Arc<dyn Music>, Rc<dyn AccountService>, Rc<dyn Playback>, Arc<dyn EqualizerService>);

fn on_engine(config: &Arc<Config>) -> Services {
    let web = Arc::new(amui::web::Web::new(config.clone()));
    let api = Arc::new(amui::api::MusicApi::new(config.clone(), web.clone()));
    // requests run on worker threads: a rejected sign-in is passed to this thread
    let (rejected, told) = async_channel::unbounded::<()>();
    gtk::glib::spawn_future_local(async move {
        while told.recv().await.is_ok() {
            ctx().account.set_state(AccountState::Reconnect);
        }
    });
    let on_rejected = Box::new(move || {
        let _ = rejected.try_send(());
    });
    let music = crate::engine::music::EngineMusic::new(config, api.clone(), web.clone(), on_rejected);
    // the downmix profiles and the equalizer are the engine's own, run in the output's chain:
    // one of each for the player and the Equalizer dialog
    let downmix = Arc::new(amui::downmix::Downmix::in_app(config.clone()));
    let equalizer = Arc::new(amui::eq::Equalizer::new(downmix.clone()));
    {
        // the chain starts with the equalizer as it was saved (this sets nothing outside the
        // application)
        let equalizer = equalizer.clone();
        std::thread::spawn(move || {
            let _ = equalizer.apply(0.0);
        });
    }
    let equalizer: Arc<dyn EqualizerService> = Arc::new(crate::engine::equalizer::EngineEqualizer::new(equalizer));
    let made = crate::engine::playback::EnginePlayback::new(config.clone(), web.clone(), api, downmix);
    let playback: Rc<dyn Playback> = match made {
        Ok(playback) => Rc::new(playback),
        Err(error) => Rc::new(crate::engine::playback::Unavailable::new(error.to_string())),
    };
    (Arc::new(music), Rc::new(crate::engine::account::EngineAccount::new(web)), playback, equalizer)
}

/// Load the library when there is a sign-in, ask Apple whether it still holds, and load again
/// after a new sign-in.
fn follow_account(ctx: &Rc<Ctx>) {
    if ctx.account.state() != AccountState::SignedOut {
        ctx.library.load(&ctx.music);
        ctx.account.check();
    }
    let weak = Rc::downgrade(ctx);
    ctx.account.signed_in.connect_forever(move |_| {
        let Some(ctx) = weak.upgrade() else { return };
        ctx.music.refresh();
        ctx.library.reload(&ctx.music);
        ctx.refreshed.emit(&());
    });
}

pub fn activate(application: &adw::Application, options: &Options) {
    if let Some(window) = application.active_window() {
        window.present();
        return;
    }
    let mut config = Config::from_process();
    // this program is the control helper of the lossless route too (`am-ui --wrapper`)
    if let Ok(program) = std::env::current_exe() {
        config.run_helper_as(program, &["--wrapper"]);
    }
    let config = Arc::new(config);
    // the engine's library is kept between runs; the demonstration's is made anew
    let library = Rc::new(if options.demo {
        Library::default()
    } else {
        Library::with_snapshots(crate::library::Snapshots::new(config.state_dir.join("library")))
    });
    // the demonstration and screenshots keep their settings to themselves
    let settings = Settings::load(&config, !(options.demo || options.screenshot.is_some()));
    crate::prefs::apply_appearance(&settings.text("appearance", "system"));
    if let (Some(scale), Some(gtk_settings)) = (options.text_scale, gtk::Settings::default()) {
        // what the desktop's text scaling factor sets: the resolution of text, in 1024ths of a
        // dot per inch
        gtk_settings.set_gtk_xft_dpi((96.0 * 1024.0 * scale).round() as i32);
    }
    load_style();
    let (music, account, playback, equalizer): Services = if options.demo {
        use crate::equalizer::{DemoEqualizer, DemoStart};
        let music = Arc::new(DemoMusic::new(&config.root));
        let equalizer = Arc::new(DemoEqualizer::new(DemoStart::of_show(options.show.as_deref())));
        (music, Rc::new(DemoAccount::new(!options.signed_out)), Rc::new(DemoPlayback::new()), equalizer)
    } else {
        on_engine(&config)
    };
    ArtLoader::install(music.clone());
    let search = Search::new(options.search_scope.unwrap_or_default(), &library);
    let ctx = Rc::new(Ctx {
        options: options.clone(),
        player: Player::new(playback, music.clone()),
        equalizer,
        music,
        library,
        favorites: Rc::new(Favorites::default()),
        nav: Navigator::new(),
        search,
        toast: Signal::new(),
        show_now_playing: Signal::new(),
        show_queue: Signal::new(),
        playlist_changed: Signal::new(),
        playlist_deleted: Signal::new(),
        refreshed: Signal::new(),
        settings: settings.clone(),
        account: Account::new(account),
    });
    CTX.with(|slot| *slot.borrow_mut() = Some(ctx.clone()));
    let window = window::build(application, &ctx);
    crate::shortcuts::install(application, &window);
    install_app_actions(application, &window);
    remember(&ctx, &window);
    follow_account(&ctx);
    window.present();
    crate::mpris::start(
        application,
        &application.application_id().map(|id| id.to_string()).unwrap_or_else(|| crate::APP_ID.to_string()),
    );
    if options.playing {
        // a song from the library as soon as it is there
        let started = std::cell::Cell::new(false);
        let weak = Rc::downgrade(&ctx);
        ctx.library.songs.connect_items_changed(move |songs, _, _, _| {
            let Some(ctx) = weak.upgrade() else { return };
            if songs.n_items() > 20 && !started.replace(true) {
                let tracks: Vec<_> =
                    (9..24).filter_map(|i| songs.item(i).as_ref().and_then(crate::library::track_of)).collect();
                ctx.player.playback.play(tracks, ctx.options.song.unwrap_or(2));
                if let Some(seconds) = ctx.options.seek {
                    ctx.player.playback.seek(seconds);
                }
            }
        });
    }
    if options.paused {
        // the song stops the moment it plays, where `--seek` put it
        let paused = std::cell::Cell::new(false);
        let weak = Rc::downgrade(&ctx);
        ctx.player.changed.connect_forever(move |_| {
            let Some(ctx) = weak.upgrade() else { return };
            if ctx.player.is_playing() && !paused.replace(true) {
                ctx.player.playback.toggle();
            }
        });
    }
    for (after, action) in options.script.clone() {
        let (window, told) = (window.clone(), options.activity);
        gtk::glib::timeout_add_local_once(std::time::Duration::from_secs_f64(after), move || {
            if told {
                println!("{after} s: {action}");
            }
            if gtk::prelude::WidgetExt::activate_action(&window, &action, None).is_err() {
                eprintln!("am-ui: there is no action {action}");
            }
        });
    }
    if options.activity {
        crate::widgets::activity::report(&window);
    }
    match options.show.as_deref() {
        Some("preferences") => crate::prefs::show_preferences(&window),
        Some("about") => crate::prefs::show_about(&window),
        Some("new-playlist") => crate::dialogs::new_playlist(&window, Vec::new()),
        Some("sign-in") => crate::widgets::signin::show(&window),
        Some("sign-in-code") => {
            crate::widgets::signin::show_step(&window, crate::account::SignInStep::Code { notice: None })
        }
        Some("sign-in-busy") => {
            crate::widgets::signin::show_step(&window, crate::account::SignInStep::Busy("Signing in…".into()))
        }
        Some("sign-in-failed") => crate::widgets::signin::show_step(
            &window,
            crate::account::SignInStep::Failed("Your Apple Account or password was incorrect".into()),
        ),
        Some("shortcuts") => {
            let _ = gtk::prelude::WidgetExt::activate_action(&window, "win.shortcuts", None);
        }
        Some(name) if name.starts_with("equalizer") => crate::widgets::equalizer::show_scene(&window, name),
        _ => {}
    }
    if let Some(target) = &options.open {
        crate::pages::detail::open(target);
    }
    if options.now_playing {
        ctx.show_now_playing.emit(&true);
    }
    if options.queue {
        ctx.show_queue.emit(&true);
    }
    if let Some(path) = options.screenshot.clone() {
        shot::when_settled(&window, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Options, String> {
        Options::parse(args.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn options_for_checks_without_a_screen() {
        let options = parse(&[
            "--demo",
            "--playing",
            "--song",
            "0",
            "--seek",
            "20.5",
            "--paused",
            "--activity",
            "--at",
            "3",
            "win.queue",
            "--at",
            "7.5",
            "app.quit",
        ])
        .unwrap();
        assert_eq!((options.song, options.seek, options.paused, options.activity), (Some(0), Some(20.5), true, true));
        assert_eq!(options.script, vec![(3.0, "win.queue".to_string()), (7.5, "app.quit".to_string())]);
        // without them the demonstration starts its third song from the beginning
        let plain = parse(&["--demo", "--playing"]).unwrap();
        assert_eq!((plain.song, plain.seek, plain.paused, plain.script.len()), (None, None, false, 0));
        let wrong: [&[&str]; 5] =
            [&["--seek", "soon"], &["--seek", "-4"], &["--at", "3"], &["--at", "later", "app.quit"], &["--song"]];
        for arguments in wrong {
            assert!(parse(arguments).is_err(), "{arguments:?}");
        }
    }

    #[test]
    fn an_option_for_larger_text() {
        assert_eq!(parse(&["--demo", "--text-scale", "1.5"]).unwrap().text_scale, Some(1.5));
        assert_eq!(parse(&["--demo"]).unwrap().text_scale, None);
        let wrong: [&[&str]; 4] =
            [&["--text-scale"], &["--text-scale", "large"], &["--text-scale", "0"], &["--text-scale", "9"]];
        for arguments in wrong {
            assert!(parse(arguments).is_err(), "{arguments:?}");
        }
    }

    #[test]
    fn options_for_the_order_and_the_view_of_the_library() {
        use crate::pages::view_options::View;
        use crate::sorting::Order;
        let options = parse(&["--demo", "--page", "albums", "--sort", "release-date", "--view", "list"]).unwrap();
        assert_eq!((options.sort, options.view), (Some(Order::ReleaseDate), Some(View::List)));
        let plain = parse(&["--demo"]).unwrap();
        assert_eq!((plain.sort, plain.view), (None, None));
        let wrong: [&[&str]; 4] = [&["--sort", "Title"], &["--sort"], &["--view", "table"], &["--view"]];
        for arguments in wrong {
            assert!(parse(arguments).is_err(), "{arguments:?}");
        }
    }
}
