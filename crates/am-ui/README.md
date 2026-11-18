# am-ui: the application

A native Apple Music client for the GNOME desktop: GTK 4 and libadwaita on the `amui` engine.
It offers what Apple Music offers (library, catalog, playlists, radio, lyrics, a queue) in a
window that is a GNOME window: native header bars and window controls, a sidebar, dialogs and
popovers of libadwaita, styled toward Apple Music's palette and typography.

```sh
cargo run -p am-ui -- --demo                       # generated data, no account, no sound
cargo run -p am-ui -- --demo --page albums --playing
cargo run -p am-ui -- --demo --open playlist:p.0
crates/am-ui/tools/headless.sh target/debug/am-ui --demo --page songs --playing --size 1440x900 --screenshot /tmp/songs.png
crates/am-ui/tools/headless.sh target/debug/am-ui --demo --open album:l.5 --scroll 400 --size 900x700 --screenshot /tmp/album.png
crates/am-ui/tools/headless.sh target/debug/am-ui --demo --page albums --sort artist --view list --size 900x700 --screenshot /tmp/albums.png
```

`--page NAME` starts on a page (`home new radio recent artists albums songs playlists search`),
`--open KIND:ID` opens the page of an album, a playlist or an artist (`album:l.3`, `album:al200`,
`playlist:p.0`, `playlist:pl.3`, `artist:ar1234`, or `artist:NAME` for an artist of the library;
ids that begin with `l.`, `p.` and `r.` are the library's) and `--scroll PIXELS` (or `end`)
scrolls that page down. `--search TERM` starts with TERM in the search field and what it finds on
the Search page (`--search-scope library` shows the results of Your Library first; an Apple Music
link as TERM is opened, as when it is pasted). `--sort KEY` puts the library's pages in an order
instead of the one chosen last (`title`, `artist`, `album`, `recently-added`, `release-date`, on
the pages that offer it), `--view grid` or `--view list` shows Albums, Artists and Playlists so,
and `--show view-options` opens the "View Options" menu of the page given with `--page`.
`--playing` starts a song, `--now-playing` and
`--queue` open those views, `--size WxH` sets the window size, `--text-scale FACTOR` makes text
FACTOR times its size as the desktop's text scaling factor does (1.25, 1.5, 2) and
`--screenshot FILE.png` draws the window into a file once it has loaded and quits.
`tools/headless.sh` runs any of that inside a private headless compositor (mutter, or KWin's
virtual backend where there is no mutter): nothing appears on the real display. Develop and check
the user interface that way. Mutter takes its scale from the desktop's settings (`scaling-factor`,
2 on a desktop set to 200%; with `GSETTINGS_BACKEND=memory` in front of the command it runs at scale
1 and the application with GNOME's default font); KWin runs a monitor of 2560x1440 or more at scale
2 and a smaller one at scale 1.

Under KWin a run can also be clicked: `tools/fake-input.sh` builds `target/tools/fake-input`, which
reads commands (`click X Y`, `right X Y`, `move X Y`, `press`, `release`, `scroll N`, `key CODE`,
`sleep S`) from its input and drives a real pointer in the private compositor. With the window as
large as the monitor, the coordinates are the window's:

```sh
crates/am-ui/tools/fake-input.sh
crates/am-ui/tools/headless.sh -s 1400x850 bash -c '
  AMUI_SHOT_DELAY_MS=4000 target/debug/am-ui --demo --page albums --size 1400x850 --screenshot /tmp/shot.png &
  printf "sleep 1.5\nclick 1179 814\nsleep 3\n" | target/tools/fake-input; wait'
```

The demonstration has what the pages need to show: library albums `l.0` to `l.159` (every fifth
with guest artists on some songs, `l.5` and every twentieth after it on several discs, every
seventh a single), catalog albums up to `al999`, the 500 songs of `p.0` (not editable), the
editable playlists `p.1` to `p.9` (`p.5` with a long description, every third without artwork),
the empty `p.12`, and catalog playlists `pl.N`. `AMUI_DEMO_DELAY_MS` sets how long its answers
take (40 by default; a few thousand show the spinners), `AMUI_DEMO_SEARCH_MS` how long its
searches take (twice that delay for Apple Music by default; a few thousand show what the library
holds for a term before Apple Music's results) and `AMUI_SHOT_DELAY_MS` when the screenshot is
taken. `AMUI_DEMO_SONGS=N` gives the library at least N songs (copies of its own with titles,
albums and dates of their own), for checks of long lists.

`--playing` starts the third of fifteen songs. `--song N` (0 to 14) starts another one: their
lyrics differ (2, 4, 7, 10 and 13 have word timing, 3, 5, 11 and 14 line timing, 1, 6, 9 and 12
no timing, 0 and 8 no lyrics; 14 is the last, so nothing plays next). With word timing, words of
seven letters or more are sung in two syllables and every third line ends on a held word; a line
begins every 4.5 seconds from 6 seconds on. `--seek SECONDS` starts the song at a position and
`--paused` pauses it there. `--at SECONDS ACTION` activates an action of the window by itself, so
that a run without a pointer can do what a click does (`win.play-pause`, `win.next`,
`win.now-playing`, `win.queue`, `win.fullscreen`, `win.go-to-song` for the song that plays,
`app.quit`; the option can be given several times), and `--activity` prints once a second how
many frames the window drew and which tick callbacks and timers of Now Playing, the lyrics and
the queue ran:

```sh
crates/am-ui/tools/headless.sh target/debug/am-ui --demo --playing --song 2 --seek 20 --activity \
    --at 3 win.now-playing --at 8 win.play-pause --at 13 win.play-pause --at 18 win.now-playing --at 22 app.quit
```

## Structure

| file | what |
|---|---|
| `main.rs`, `app.rs` | options, start-up, `Ctx` (what all widgets share; `app::ctx()` on the UI thread) |
| `data.rs` | `Music`: what pages ask of the music service (blocking, called on worker threads) |
| `playback.rs`, `player.rs` | `Playback`: commands to the player; `Player`: its last known state, queue, lyrics, with signals |
| `demo.rs`, `demo_playback.rs` | generated library and a simulated player for `--demo` |
| `engine/` | the same interfaces on the engine: `music.rs` (Apple's service), `account.rs` (the sign-in), `equalizer.rs` |
| `account.rs`, `widgets/signin.rs` | whether this machine is signed in, the sign-in dialog (Apple Account, password, code) |
| `equalizer.rs`, `widgets/equalizer.rs`, `widgets/eq_graph.rs` | the equalizer's service, its dialog and the response graph |
| `regions.rs` | Apple Music's regions with their names, for the catalog region in Preferences |
| `signal.rs` | `Signal<T>` (listeners that end with their widget) and `background(work, done)` |
| `library.rs`, `favorites.rs` | the library's list models; which songs are favourites |
| `finder.rs` | what the library holds for a search term, found in those lists as the term is typed, best match first |
| `sorting.rs` | the orders of the library's lists, and the keys that put rows in them (titles as people expect them) |
| `actions.rs` | what can be done with a song, album, playlist, artist or station, wherever it appears |
| `nav.rs` | destinations, the page history, back and forward |
| `links.rs` | which song, album, playlist or artist an Apple Music link names (pasted into the search field) |
| `art.rs` | artwork fetched and decoded on worker threads, kept as textures |
| `scroll.rs` | scrolling in step with the display (see below) |
| `window.rs`, `sidebar.rs` | the window and its sidebar |
| `pages/` | one module per kind of page; `pages::build(dest)` makes the page of a destination |
| `pages/detail.rs` | what the pages of an album, a playlist and an artist share: `Frame` (content, spinner, problem), the page cache, `--open` |
| `pages/collection.rs`, `pages/artist.rs` | the pages of an album and of a playlist; the page of an artist |
| `pages/view_options.rs` | the library's Albums, Artists, Playlists and Songs: sorted as chosen ("Sort By"), as a grid or a list ("View As") |
| `widgets/` | `Card`, `TrackRow`, `ItemRow`, `Thumb`, `SeekBar`, the player bar, the sound menu... |
| `widgets/embedded.rs` | `Embedded`: a list or grid view as one part of a page that scrolls as a whole |
| `widgets/hero.rs`, `cover.rs`, `excerpt.rs`, `columns.rs` | the header of a page, its picture, a text with "more", rows in two columns |
| `widgets/stage.rs` | what stands above the player bar: sidebar and pages, Now Playing over them, the queue beside them |
| `widgets/now_playing.rs`, `glow.rs`, `lyrics.rs` | Now Playing: the artwork on a glow of its colours, and the lyrics that follow the song |
| `widgets/queue.rs` | Playing Next: the list follows the player's queue by changing only the rows that changed |
| `widgets/activity.rs` | counts what moves by itself in those views, for `--activity` |
| `style.css`, `style-dark.css` | the look; the second holds what differs in the dark appearance |
| `resources/` | the application's own symbolic icons (`am-*-symbolic`), packed into the program |

## Rules

**Threads.** Widgets live on the UI thread. Anything that can take time (every `Music` call, file
access, decoding) runs through `signal::background(work, done)`; `done` runs on the UI thread.
`Playback` commands return at once and answer through `Player`'s signals. Never block the UI
thread, and never call a `Music` method on it.

**State and signals.** Shared state is in `Ctx` (`ctx().player`, `ctx().library`,
`ctx().favorites`, `ctx().nav`). A widget that shows shared state subscribes with
`signal.connect(&widget, |widget, value| ...)`: the subscription holds the widget weakly and ends
with it. Do not keep `Rc<Ctx>` inside long-lived closures of a widget that `Ctx` owns; call
`crate::app::ctx()` inside the closure.

**A new image on every refresh.** At 240 Hz a frame has 4.2 ms. Measurements made while the
toolkit was chosen led to these rules:

- Every scrolling area gets `scroll::SmoothScroll` (use `pages::scrolled(child)`): it takes the
  scroll events and moves the content from a tick callback, so touchpad, fling and wheel show a
  new position every frame. Do not use bare `gtk::ScrolledWindow` for content that scrolls
  vertically.
- Long or unbounded lists are `gtk::ListView` / `gtk::GridView` with recycled items
  (`pages::songs::track_list`, `pages::grid::card_grid`, `pages::view_options::item_list`). A row
  may be built from ordinary widgets (`TrackRow`, `ItemRow`). A card of a grid or shelf must be a
  single widget that draws its own text (`Card`): cards made of labels cost too much when a grid
  recycles a row of them per frame.
- A list that is sorted is sorted without holding the window: a `gtk::SortListModel` with one
  key per row (`sorting`), and a new order sorted in the background a millisecond at a time
  (`pages::view_options`).
- A list that shares its page with something else (a header above it, a footer, a second list)
  does not get a scrolled window of its own. The page is one column in a `gtk::Viewport`
  (`pages::detail::Frame`), and the list goes into it as `widgets::embedded::Embedded`, which
  gives the list only the part of itself that is in view: it keeps its recycled rows, and the
  header scrolls away with them. A list view laid out in full inside a viewport would show its
  first two hundred rows and nothing after them.
- No blur, no backdrop filters, no full-window gradients drawn per frame. A glow is a small
  texture computed on the CPU and drawn scaled up (as the prototype's Now Playing does).
- A tick callback or a timer runs only while something moves; an idle window draws nothing.
  Progress that moves a pixel a second is updated a few times a second, not per frame.
- Artwork goes through `art::loader()` at the size it is drawn (`Thumb`, `Card` do that): never
  decode on the UI thread, never keep full-size images.

**Look.** GNOME structure, Apple Music palette: the accent is Apple Music's red, the dark
appearance uses its near-black surfaces, the type is the system's sans (Adwaita Sans). Colours
come from libadwaita's variables (`var(--accent-color)`, `currentColor` with `alpha()`), never
hard-coded in Rust except white on artwork. Custom-drawn widgets take `widget.color()` as their
text colour. Use libadwaita's widgets for standard things (dialogs, toasts, preference rows,
status pages, spinners) and its style classes (`flat`, `circular`, `pill`, `dim-label`,
`numeric`). Icons: Adwaita's symbolic icons, or a new `am-NAME-symbolic.svg` in
`resources/icons/scalable/actions/` (16x16, filled paths only) listed in
`resources/resources.gresource.xml`.

**Text size.** Text follows the desktop's text scaling factor and font, and nothing that holds
text may cut it off. Font sizes in the style are in `em`, never in `px`; text drawn in Rust uses
`widgets::font_layout` (a size relative to the widget's font). A widget that places its children
by hand takes its heights and the widths of its columns from the fonts of its labels
(`widgets::label_line`, `label_width`, kept until `font_serial` changes), so that the rows of a
list are all of one height, which grows with the text. Widths chosen to hold some text (a card, a
column of songs) grow with `widgets::text_scaling`. Look at what was touched with `--text-scale`
1.25, 1.5 and 2 as well.

**Text for the user.** Sentence in Apple Music's words where it has them ("Play Next", "Add to
Library", "Recently Added"), title case for buttons and menu items. Problems are shown as toasts
(`ctx().toast.emit(...)`) or in the page (`adw::StatusPage`), never printed only.

**Privacy.** Song titles, artist names, account details and tokens are never logged or printed.
Fixtures and demo data are synthetic.

**Checks.** `cargo build -p am-ui` without warnings in new code, `cargo clippy -p am-ui`,
`cargo fmt --all`, `cargo test -p am-ui`, and a headless screenshot of every page or state that
was touched, looked at before the work is called done. What needs a window to be checked (how
`Embedded` places the rows of a list while its page scrolls) is one test that only runs in the
headless compositor: `crates/am-ui/tools/headless.sh cargo test -p am-ui -- --ignored display`.
What moves by itself is checked with `--activity` (above): the callbacks of a view count zero
while it is hidden, and with playback paused the window draws no frame at all.
