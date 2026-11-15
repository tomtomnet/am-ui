//! The main window: sidebar, pages, player bar.

use std::rc::Rc;

use adw::prelude::*;

use crate::account::AccountState;
use crate::app::Ctx;
use crate::nav::Dest;
use crate::widgets::{player_bar, stage};
use crate::{pages, sidebar};

/// Shown across the window, over the player bar, while there is no sign-in Apple accepts.
fn account_banner(ctx: &Rc<Ctx>) -> adw::Banner {
    let banner = adw::Banner::new("");
    banner.set_button_label(Some("Sign In"));
    banner.connect_button_clicked(crate::widgets::signin::show);
    let show = |banner: &adw::Banner, state: AccountState| {
        let text = match state {
            AccountState::SignedOut => "Sign in to Apple Music to see your library and play music",
            AccountState::Reconnect => "Apple Music no longer accepts this sign-in: sign in again",
            AccountState::Connected | AccountState::Unknown => "",
        };
        if !text.is_empty() {
            banner.set_title(text);
        }
        banner.set_revealed(!text.is_empty());
    };
    show(&banner, ctx.account.state());
    ctx.account.changed.connect(&banner, move |banner, state| show(banner, *state));
    banner
}

pub fn build(application: &adw::Application, ctx: &Rc<Ctx>) -> adw::ApplicationWindow {
    let nav = ctx.nav.clone();
    nav.set_builder(pages::build);
    pages::view_options::prepare();
    let sidebar = Rc::new({
        let nav = nav.clone();
        sidebar::build(move |dest| nav.go(dest))
    });
    {
        // the sidebar follows the history
        let shown = sidebar.clone();
        nav.changed.connect_forever(move |dest| shown.show(dest));
        sidebar.follow_playlists();
    }
    let split = adw::OverlaySplitView::new();
    split.set_sidebar(Some(&sidebar.widget));
    split.set_content(Some(&nav.view));
    split.set_min_sidebar_width(220.0);
    split.set_max_sidebar_width(260.0);
    split.set_vexpand(true);

    let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
    // Now Playing lies over the sidebar and the pages, the queue stands beside them
    layout.append(&stage::build(&split));
    // under the pages, so that the header bars and the window's buttons stay where they are
    layout.append(&account_banner(ctx));
    layout.append(&player_bar::build());

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&layout));
    {
        let toasts = toasts.clone();
        ctx.toast.connect_forever(move |message| {
            // the text is shown as it is: a song's title may hold "&" or "<"
            toasts.add_toast(adw::Toast::builder().title(message.as_str()).use_markup(false).timeout(3).build())
        });
        let toast = ctx.clone();
        ctx.favorites.failed.connect_forever(move |message| toast.toast.emit(message));
        let toast = ctx.clone();
        ctx.player.problem.connect_forever(move |message| toast.toast.emit(message));
    }

    let (width, height) = ctx.options.size.unwrap_or((1280, 820));
    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("Music")
        .default_width(width)
        .default_height(height)
        .content(&toasts)
        .build();
    window.add_css_class("am-ui");
    nav.go(ctx.options.page.clone().unwrap_or(Dest::Home));
    // the keyboard starts in the page, not in the search field
    gtk::prelude::GtkWindowExt::set_focus(&window, Some(&nav.view));
    pages::search::attach(&window, &sidebar.search);
    window
}
