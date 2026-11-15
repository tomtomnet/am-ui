//! `amui <command>` for the session, the library, search, metadata and lyrics
//! (`amui/cli_library.py`).
//!
//! Plain-text output, exit status 0 on success and 1 on failure. Library item names are printed
//! to the terminal only (never written to files). A rejected session prints a reconnect hint.
//!
//! The pure parts are public: [`clip`], [`table`], [`mmss`], [`hms`].

use std::collections::BTreeSet;

use crate::api::{ApiError, MusicApi};
use crate::session::{self, SessionError, State, Status};
use crate::types::{Album, Artist, Item, Page, Playlist, SearchResults, Track};
use crate::web::Source;

use super::args::{Arg, Opt, Parsed, Spec};
use super::text::{shown, truth};
use super::{Command, Console, Context};

pub const RECONNECT_HINT: &str =
    "Sign-in rejected: sign in again with the Sign In button of the app, or `./am-ui cli connect` in a terminal";

// --- helpers ------------------------------------------------------------------

/// Why a command of this module failed.
enum Failure {
    Api(ApiError),
    Session(SessionError),
}

impl From<ApiError> for Failure {
    fn from(error: ApiError) -> Failure {
        Failure::Api(error)
    }
}

impl From<SessionError> for Failure {
    fn from(error: SessionError) -> Failure {
        Failure::Session(error)
    }
}

/// Run a command's body on its parsed arguments. A rejected or missing sign-in prints the
/// reconnect hint, any other failure `Error: ...`; both end with status 1 (Python's `_guarded`).
fn guarded(
    spec: &Spec,
    args: &[String],
    console: &mut Console,
    body: impl FnOnce(&Parsed, &mut Console) -> Result<i32, Failure>,
) -> i32 {
    let parsed = match spec.parse(args) {
        Ok(parsed) => parsed,
        Err(stop) => return console.stop(&stop),
    };
    match body(&parsed, console) {
        Ok(status) => status,
        Err(Failure::Api(error)) if error.is_auth() => {
            console.error(RECONNECT_HINT);
            1
        }
        Err(Failure::Api(error)) => {
            console.error(format!("Error: {error}"));
            1
        }
        Err(Failure::Session(error)) => {
            console.error(format!("Error: {error}"));
            1
        }
    }
}

fn api(context: &Context) -> MusicApi {
    MusicApi::new(context.config.clone(), context.http.clone())
}

/// A cell of at most `width` characters on one line: a longer text ends in "…".
pub fn clip(text: &str, width: usize) -> String {
    let text = text.replace('\n', " ");
    if text.chars().count() <= width {
        return text;
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// The lines of a table: the headers, a rule, the rows; every column as wide as its widest cell
/// (cells clipped to `widths`, 48 characters where that names no width). `  (none)` without
/// rows.
pub fn table(headers: &[&str], rows: &[Vec<String>], widths: &[usize]) -> Vec<String> {
    if rows.is_empty() {
        return vec!["  (none)".to_string()];
    }
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            let clipped = |(column, cell): (usize, &String)| clip(cell, widths.get(column).copied().unwrap_or(48));
            row.iter().enumerate().map(clipped).collect()
        })
        .collect();
    let length = |text: &str| text.chars().count();
    let sizes: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(column, header)| {
            let cells = rows.iter().filter_map(|row| row.get(column)).map(|cell| length(cell));
            cells.fold(length(header), usize::max)
        })
        .collect();
    /// The cells of one line, each filled up to its column's size (the last one too).
    fn joined<'a>(cells: impl Iterator<Item = &'a str>, sizes: &[usize]) -> String {
        let padded = cells.zip(sizes).map(|(cell, size)| format!("{cell:<width$}", width = *size));
        padded.collect::<Vec<_>>().join("  ")
    }
    let mut lines = vec![joined(headers.iter().copied(), &sizes)];
    lines.push(sizes.iter().map(|size| "-".repeat(*size)).collect::<Vec<_>>().join("  "));
    lines.extend(rows.iter().map(|row| joined(row.iter().map(String::as_str), &sizes)));
    lines
}

fn print_table(console: &mut Console, headers: &[&str], rows: &[Vec<String>], widths: &[usize]) {
    for line in table(headers, rows, widths) {
        console.line(line);
    }
}

/// Milliseconds as `m:ss`; empty for an unknown length.
pub fn mmss(ms: Option<i64>) -> String {
    let Some(ms) = ms else { return String::new() };
    let seconds = ms.div_euclid(1000);
    format!("{}:{:02}", seconds.div_euclid(60), seconds.rem_euclid(60))
}

/// Seconds as `mm:ss.sss`, the time of a lyric line; a line without a time gets a blank of the
/// same width.
pub fn hms(seconds: Option<f64>) -> String {
    let Some(seconds) = seconds else { return "   --   ".to_string() };
    let seconds = if seconds > 0.0 { seconds } else { 0.0 };
    let rest = seconds % 60.0;
    let minutes = ((seconds - rest) / 60.0).round();
    format!("{:02}:{rest:06.3}", minutes as i64)
}

fn atmos(track: &Track) -> &'static str {
    match track.atmos_known() {
        None => "?",
        Some(true) => "atmos",
        Some(false) => "-",
    }
}

/// A count as Python's `str(count or "")` has it: nothing for none and for zero.
fn count(value: Option<i64>) -> String {
    value.filter(|count| *count != 0).map(|count| count.to_string()).unwrap_or_default()
}

/// What a listing shows of an entity: key, name, artist, count.
trait Entity {
    fn row(&self) -> Vec<String>;
}

impl Entity for Album {
    fn row(&self) -> Vec<String> {
        vec![self.key(), self.name.clone(), self.artist.clone(), count(self.track_count)]
    }
}

impl Entity for Playlist {
    fn row(&self) -> Vec<String> {
        let curator = self.curator.as_deref().filter(|curator| !curator.is_empty());
        let by = curator.unwrap_or(if self.is_library { "library" } else { "" });
        vec![self.key(), self.name.clone(), by.to_string(), count(self.track_count)]
    }
}

impl Entity for Artist {
    fn row(&self) -> Vec<String> {
        vec![self.key(), self.name.clone(), String::new(), String::new()]
    }
}

impl Entity for Track {
    fn row(&self) -> Vec<String> {
        vec![self.key(), self.name.clone(), self.artist.clone(), mmss(self.duration_ms)]
    }
}

impl Entity for Item {
    fn row(&self) -> Vec<String> {
        match self {
            Item::Track(track) => track.row(),
            Item::Album(album) => album.row(),
            Item::Playlist(playlist) => playlist.row(),
            Item::Artist(artist) => artist.row(),
            // no listing of these commands holds one; the Python client knew no stations
            Item::Station(station) => {
                vec![station.id.clone(), station.name.clone(), station.subtitle.clone(), String::new()]
            }
        }
    }
}

const ENTITY_HEADERS: [&str; 4] = ["key", "name", "artist", "count"];
const ENTITY_WIDTHS: [usize; 4] = [24, 48, 32, 6];

fn print_entities<T: Entity>(console: &mut Console, entities: &[T]) {
    let rows: Vec<Vec<String>> = entities.iter().map(Entity::row).collect();
    print_table(console, &ENTITY_HEADERS, &rows, &ENTITY_WIDTHS);
}

/// "; next --offset N" when a page has a next one.
fn more(next_offset: Option<i64>) -> String {
    next_offset.map(|offset| format!("; next --offset {offset}")).unwrap_or_default()
}

/// " of N" when a page names the total.
fn of(total: Option<i64>) -> String {
    total.map(|total| format!(" of {total}")).unwrap_or_default()
}

fn print_page<T: Entity>(console: &mut Console, page: &Page<T>, kind: &str) {
    print_entities(console, &page.items);
    console.line(format!("{} {kind}{}{}", page.items.len(), of(page.total), more(page.next_offset)));
}

fn print_tracks(console: &mut Console, tracks: &[Track], start: i64) {
    let rows: Vec<Vec<String>> = tracks
        .iter()
        .zip(start..)
        .map(|(track, number)| {
            let catalog_id = track.catalog_id.as_deref().filter(|id| !id.is_empty()).unwrap_or("(unavailable)");
            vec![
                number.to_string(),
                track.name.clone(),
                track.artist.clone(),
                mmss(track.duration_ms),
                catalog_id.to_string(),
                atmos(track).to_string(),
            ]
        })
        .collect();
    print_table(console, &["#", "name", "artist", "time", "catalog id", "audio"], &rows, &[4, 48, 32, 6, 14, 6]);
}

/// The state of the sign-in in use. Never a token: the status holds none.
fn print_status(console: &mut Console, status: &Status) {
    console.line(format!("state: {}", status.state));
    let times = [
        ("checked_at", &status.checked_at),
        ("cookie_expires_at", &status.cookie_expires_at),
        ("captured_at", &status.captured_at),
    ];
    for (key, value) in times {
        if let Some(value) = value {
            console.line(format!("{key}: {value}"));
        }
    }
    if let Some(http_status) = status.http_status {
        console.line(format!("http_status: {http_status}"));
    }
    for (kind, info) in status.library.iter().flatten() {
        if let Some(info) = info.as_object() {
            let (accessible, has_more) = (shown(info.get("accessible")), shown(info.get("has_more")));
            console.line(format!("library {kind}: accessible={accessible} has_more={has_more}"));
        }
    }
    console.line(format!("token file present: {}", truth(status.has_token)));
    if let Some(signin) = status.signin {
        let which = if signin == Source::Android { "Apple's native sign-in" } else { "browser session" };
        console.line(format!("sign-in in use: {which}"));
    }
}

/// 0 for a sign-in Apple accepts, 1 for anything else.
fn connected(status: &Status) -> i32 {
    if status.state == State::Connected { 0 } else { 1 }
}

const fn plain(name: &'static str, description: &'static str) -> Spec {
    Spec { name, description, options: &[], arguments: &[], exclusive: &[] }
}

// --- session ------------------------------------------------------------------

const STATUS: Spec = plain("status", "Saved Apple Music session state (offline)");

fn cmd_status(context: &Context, args: &[String], console: &mut Console) -> i32 {
    guarded(&STATUS, args, console, |_, console| {
        let status = session::status(&context.web);
        print_status(console, &status);
        Ok(connected(&status))
    })
}

const CHECK: Spec = plain("check", "Verify the saved session live (two one-item library requests)");

fn cmd_check(context: &Context, args: &[String], console: &mut Console) -> i32 {
    guarded(&CHECK, args, console, |_, console| {
        let status = session::check(&context.web)?;
        print_status(console, &status);
        if status.state == State::ReconnectRequired {
            console.error(RECONNECT_HINT);
        }
        Ok(connected(&status))
    })
}

const CONNECT: Spec = Spec {
    name: "connect",
    description: "Sign in to Apple Music: Apple's native sign-in in this terminal. It needs the lossless route, \
                  and is the sign-in that makes lossless available",
    options: &[
        Opt::switch("--browser", "not available: this engine has no browser sign-in"),
        Opt::int("--timeout", "TIMEOUT", "not used: the time limit of the browser sign-in"),
    ],
    arguments: &[],
    exclusive: &[],
};

/// What `connect --browser` answers with.
pub const NO_BROWSER_SIGNIN: &str = "connect --browser: this engine has no browser sign-in. `connect` without it is \
                                     Apple's native sign-in in this terminal, the one that also makes lossless available";

fn cmd_connect(context: &Context, args: &[String], console: &mut Console) -> i32 {
    guarded(&CONNECT, args, console, |args, console| {
        if args.flag("--browser") {
            console.error(NO_BROWSER_SIGNIN);
            return Ok(2);
        }
        if !(context.signin)(&context.web)? {
            console.error("Sign-in not completed; an earlier sign-in on this machine is kept.");
            return Ok(1);
        }
        let status = session::check(&context.web)?;
        print_status(console, &status);
        Ok(connected(&status))
    })
}

// --- library ------------------------------------------------------------------

const fn paged(name: &'static str, description: &'static str, arguments: &'static [Arg]) -> Spec {
    const PAGING: &[Opt] = &[Opt::int("--offset", "OFFSET", ""), Opt::int("--limit", "LIMIT", "")];
    Spec { name, description, options: PAGING, arguments, exclusive: &[] }
}

/// `--offset` and `--limit` of a command line, with the limit the command has without one.
fn paging(args: &Parsed, limit: i64) -> (i64, i64) {
    (args.int("--offset").unwrap_or(0), args.int("--limit").unwrap_or(limit))
}

const ALBUMS: Spec = paged("albums", "List library albums", &[]);

fn cmd_albums(context: &Context, args: &[String], console: &mut Console) -> i32 {
    guarded(&ALBUMS, args, console, |args, console| {
        let (offset, limit) = paging(args, 100);
        print_page(console, &api(context).library_albums(offset, limit, None, false)?, "albums");
        Ok(0)
    })
}

const PLAYLISTS: Spec = paged("playlists", "List library playlists", &[]);

fn cmd_playlists(context: &Context, args: &[String], console: &mut Console) -> i32 {
    guarded(&PLAYLISTS, args, console, |args, console| {
        let (offset, limit) = paging(args, 100);
        print_page(console, &api(context).library_playlists(offset, limit)?, "playlists");
        Ok(0)
    })
}

const SONGS: Spec = paged("songs", "List library songs", &[]);

fn cmd_songs(context: &Context, args: &[String], console: &mut Console) -> i32 {
    guarded(&SONGS, args, console, |args, console| {
        let (offset, limit) = paging(args, 100);
        print_page(console, &api(context).library_songs(offset, limit, false)?, "songs");
        Ok(0)
    })
}

const ARTISTS: Spec = paged("artists", "List library artists", &[]);

fn cmd_artists(context: &Context, args: &[String], console: &mut Console) -> i32 {
    guarded(&ARTISTS, args, console, |args, console| {
        let (offset, limit) = paging(args, 100);
        print_page(console, &api(context).library_artists(offset, limit, true)?, "artists");
        Ok(0)
    })
}

const RECENTLY_ADDED: Spec = paged("recently-added", "List library recently added items", &[]);

fn cmd_recently_added(context: &Context, args: &[String], console: &mut Console) -> i32 {
    guarded(&RECENTLY_ADDED, args, console, |args, console| {
        let (offset, limit) = paging(args, 25);
        print_page(console, &api(context).library_recently_added(offset, limit)?, "recently added items");
        Ok(0)
    })
}

const TRACKS: Spec = paged(
    "tracks",
    "Tracks of a library album (l.…), library playlist (p.…), catalog album (digits) or catalog playlist (pl.…)",
    &[Arg::one("key", "")],
);

fn cmd_tracks(context: &Context, args: &[String], console: &mut Console) -> i32 {
    guarded(&TRACKS, args, console, |args, console| {
        let (offset, limit) = paging(args, 100);
        let api = api(context);
        let key = args.word("key").unwrap_or("").trim();
        if key.starts_with("l.") {
            let tracks = api.library_album_tracks(key)?;
            print_tracks(console, &tracks, 1);
            console.line(format!("{} tracks", tracks.len()));
        } else if key.starts_with("p.") {
            let page = api.library_playlist_tracks(key, offset, limit)?;
            print_tracks(console, &page.items, offset + 1);
            console.line(format!("{} tracks{}{}", page.items.len(), of(page.total), more(page.next_offset)));
        } else if key.starts_with("pl.") {
            let (playlist, tracks) = api.catalog_playlist(key)?;
            let curator = playlist.curator.as_deref().unwrap_or("");
            console.line(format!("{} — {curator} ({} tracks)", playlist.name, tracks.len()));
            print_tracks(console, &tracks, 1);
        } else if !key.is_empty() && key.bytes().all(|byte| byte.is_ascii_digit()) {
            let (album, tracks) = api.catalog_album(key)?;
            let traits = Some(album.audio_traits.join(", ")).filter(|traits| !traits.is_empty());
            let released = album.release_date.as_deref().filter(|date| !date.is_empty()).unwrap_or("?");
            console.line(format!(
                "{} — {} ({released}; {} tracks; audioTraits: {})",
                album.name,
                album.artist,
                tracks.len(),
                traits.as_deref().unwrap_or("unknown")
            ));
            print_tracks(console, &tracks, 1);
        } else {
            console.error("Unrecognised key: expected l.…, p.…, pl.… or a numeric catalog album id");
            return Ok(2);
        }
        Ok(0)
    })
}

// --- search / metadata / lyrics --------------------------------------------------

const SEARCH: Spec = Spec {
    name: "search",
    description: "Search the library and the catalog",
    options: &[Opt::int("--limit", "LIMIT", "")],
    arguments: &[Arg::one_or_more("term", "")],
    exclusive: &[],
};

fn print_results(console: &mut Console, title: &str, results: &SearchResults) {
    console.line(format!("== {title}"));
    if let Some(top) = &results.top {
        console.line(format!("top result ({}): {}", top.kind(), top.row()[..3].join(" | ")));
    }
    console.line("-- songs");
    print_entities(console, &results.songs);
    console.line("-- albums");
    print_entities(console, &results.albums);
    console.line("-- artists");
    print_entities(console, &results.artists);
    console.line("-- playlists");
    print_entities(console, &results.playlists);
}

fn cmd_search(context: &Context, args: &[String], console: &mut Console) -> i32 {
    guarded(&SEARCH, args, console, |args, console| {
        let term = args.words("term").join(" ");
        let limit = args.int("--limit").unwrap_or(5);
        let api = api(context);
        print_results(console, "library", &api.search_library(&term, limit)?);
        print_results(console, "catalog", &api.search_catalog(&term, limit)?);
        Ok(0)
    })
}

const SONG: Spec = Spec {
    name: "song",
    description: "Catalog song details (incl. audioTraits)",
    options: &[],
    arguments: &[Arg::one("catalog_id", "")],
    exclusive: &[],
};

fn cmd_song(context: &Context, args: &[String], console: &mut Console) -> i32 {
    guarded(&SONG, args, console, |args, console| {
        let track = api(context).catalog_song(args.word("catalog_id").unwrap_or(""))?;
        let number = |number: Option<i64>| match number.filter(|number| *number != 0) {
            Some(number) => number.to_string(),
            None => "?".to_string(),
        };
        let flag = |flag: Option<bool>| flag.map(|flag| truth(flag).to_string());
        let traits = Some(track.audio_traits.join(", ")).filter(|traits| !traits.is_empty());
        let fields = [
            ("catalog id", track.catalog_id.clone()),
            ("name", Some(track.name.clone())),
            ("artist", Some(track.artist.clone())),
            ("album", Some(track.album.clone())),
            ("album id", track.album_catalog_id.clone()),
            ("artist id", track.artist_catalog_id.clone()),
            ("duration", Some(mmss(track.duration_ms))),
            ("track", Some(format!("{} (disc {})", number(track.track_number), number(track.disc_number)))),
            ("genre", track.genre.clone()),
            ("released", track.release_date.clone()),
            ("isrc", track.isrc.clone()),
            ("content rating", track.content_rating.clone()),
            ("has lyrics", flag(track.has_lyrics)),
            ("audioTraits", Some(traits.unwrap_or_else(|| "(none listed)".to_string()))),
            ("atmos", flag(track.atmos_known())),
            ("artwork", track.artwork.as_ref().map(|artwork| artwork.url(600))),
            ("artwork bg", track.artwork.as_ref().and_then(|artwork| artwork.bg_color.clone())),
        ];
        for (label, value) in fields {
            console.line(format!("{label:<15} {}", value.unwrap_or_default()));
        }
        Ok(0)
    })
}

const LYRICS: Spec = Spec {
    name: "lyrics",
    description: "Lyrics timing summary for a catalog song",
    options: &[Opt::switch("--show", "print the lyric lines to the terminal")],
    arguments: &[Arg::one("catalog_id", "")],
    exclusive: &[],
};

fn cmd_lyrics(context: &Context, args: &[String], console: &mut Console) -> i32 {
    guarded(&LYRICS, args, console, |args, console| {
        let Some(lyrics) = api(context).lyrics(args.word("catalog_id").unwrap_or(""))? else {
            console.line("no lyrics");
            return Ok(1);
        };
        let timed = lyrics.lines.iter().filter(|line| line.begin.is_some()).count();
        let backgrounds = lyrics.lines.iter().filter(|line| line.background).count();
        let words: usize = lyrics.lines.iter().map(|line| line.words.len()).sum();
        let agents: BTreeSet<&str> =
            lyrics.lines.iter().filter_map(|line| line.agent.as_deref()).filter(|agent| !agent.is_empty()).collect();
        let agents =
            if agents.is_empty() { "-".to_string() } else { agents.into_iter().collect::<Vec<_>>().join(", ") };
        let language = lyrics.language.as_deref().filter(|language| !language.is_empty()).unwrap_or("?");
        console.line(format!("source: {}  timing: {}  language: {language}", lyrics.source, lyrics.timing.as_str()));
        console.line(format!(
            "lines: {}  timed lines: {timed}  background lines: {backgrounds}  timed words: {words}  agents: {agents}",
            lyrics.lines.len()
        ));
        if args.flag("--show") {
            for line in &lyrics.lines {
                let marker = if line.background { "  (bg) " } else { " " };
                console.line(format!("[{}]{marker}{}", hms(line.begin), line.text));
            }
        }
        Ok(0)
    })
}

// --- home / browse ----------------------------------------------------------------

const RECENT: Spec = Spec {
    name: "recent",
    description: "Recently played albums and playlists",
    options: &[Opt::int("--limit", "LIMIT", "")],
    arguments: &[],
    exclusive: &[],
};

fn cmd_recent(context: &Context, args: &[String], console: &mut Console) -> i32 {
    guarded(&RECENT, args, console, |args, console| {
        print_entities(console, &api(context).recently_played(args.int("--limit").unwrap_or(10), 0)?);
        Ok(0)
    })
}

const HOME: Spec = plain("home", "Recently played items and recommendation groups");

fn cmd_home(context: &Context, args: &[String], console: &mut Console) -> i32 {
    guarded(&HOME, args, console, |_, console| {
        let api = api(context);
        console.line("== recently played");
        print_entities(console, &api.recently_played(10, 0)?);
        console.line("== recently played tracks");
        print_tracks(console, &api.recently_played_tracks(10, 0)?, 1);
        console.line("== recommendations");
        let rows: Vec<Vec<String>> = api
            .recommendations()?
            .iter()
            .map(|group| {
                let kinds: BTreeSet<&str> = group.items.iter().map(Item::kind).collect();
                vec![
                    group.title.clone(),
                    group.items.len().to_string(),
                    kinds.into_iter().collect::<Vec<_>>().join(", "),
                ]
            })
            .collect();
        print_table(console, &["title", "items", "kinds"], &rows, &[48, 6, 24]);
        Ok(0)
    })
}

const CHARTS: Spec = plain("charts", "Top songs, albums and playlists for the storefront");

fn cmd_charts(context: &Context, args: &[String], console: &mut Console) -> i32 {
    guarded(&CHARTS, args, console, |_, console| {
        let charts = api(context).charts()?;
        let title = |kind: &str| charts.titles.get(kind).cloned().flatten().unwrap_or_default();
        console.line(format!("== songs: {}", title("songs")));
        print_tracks(console, &charts.songs, 1);
        console.line(format!("== albums: {}", title("albums")));
        print_entities(console, &charts.albums);
        console.line(format!("== playlists: {}", title("playlists")));
        print_entities(console, &charts.playlists);
        Ok(0)
    })
}

/// The commands of this module (Python: `COMMANDS` of `amui/cli_library.py`).
pub const COMMANDS: &[Command] = &[
    Command { name: "status", help: "saved Apple Music session state (offline)", run: cmd_status },
    Command { name: "check", help: "verify the saved session live (read-only)", run: cmd_check },
    Command { name: "connect", help: "sign in to Apple Music: the native sign-in in this terminal", run: cmd_connect },
    Command { name: "albums", help: "library albums [--offset N] [--limit N]", run: cmd_albums },
    Command { name: "playlists", help: "library playlists [--offset N] [--limit N]", run: cmd_playlists },
    Command { name: "songs", help: "library songs [--offset N] [--limit N]", run: cmd_songs },
    Command { name: "artists", help: "library artists [--offset N] [--limit N]", run: cmd_artists },
    Command {
        name: "recently-added",
        help: "recently added library albums/playlists [--offset N] [--limit N]",
        run: cmd_recently_added,
    },
    Command {
        name: "tracks",
        help: "tracks of l.<album>, p.<playlist>, pl.<catalog playlist> or a catalog album id",
        run: cmd_tracks,
    },
    Command { name: "search", help: "search the library and the catalog: search <term> [--limit N]", run: cmd_search },
    Command { name: "song", help: "catalog song details incl. audioTraits: song <catalog id>", run: cmd_song },
    Command { name: "lyrics", help: "lyrics timing summary: lyrics <catalog id> [--show]", run: cmd_lyrics },
    Command { name: "recent", help: "recently played albums and playlists", run: cmd_recent },
    Command { name: "home", help: "recently played + recommendation titles", run: cmd_home },
    Command { name: "charts", help: "storefront charts (top 20 songs/albums/playlists)", run: cmd_charts },
];

#[cfg(test)]
mod tests {
    //! What the reference of the Python command line (`cli::reference`) does not hold: the live
    //! check, the sign-in, usage errors. The network of the check is a stand-in, the sign-in a
    //! closure; tokens are made up.
    use super::*;
    use crate::cli::testing::{Scratch, reference_network};
    use crate::types::Station;
    use crate::web::testing::{ANDROID_TOKEN, BROWSER_TOKEN, sign_in};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The lines of a status with the time of the check taken out.
    fn lines_without_the_time(printed: &str) -> Vec<String> {
        let checked = regex::Regex::new(r"^checked_at: \d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d{6})?\+00:00$").unwrap();
        printed.lines().map(|line| checked.replace(line, "checked_at: <now>").into_owned()).collect()
    }

    #[test]
    fn check_asks_apple_and_prints_the_state() {
        let scratch = Scratch::new();
        // no sign-in: nothing to ask
        let ran = scratch.run(&["check"]);
        let nothing = "state: no_session\ntoken file present: False\n";
        assert_eq!((ran.status, ran.out.as_str(), ran.err.as_str()), (1, nothing, ""));
        assert!(scratch.wire.lock().unwrap().is_empty());

        sign_in(&scratch.config, Source::Android, ANDROID_TOKEN);
        *scratch.verdict.lock().unwrap() = 200;
        let ran = scratch.run(&["check"]);
        assert_eq!(
            lines_without_the_time(&ran.out),
            [
                "state: connected",
                "checked_at: <now>",
                "library playlists: accessible=True has_more=True",
                "library songs: accessible=True has_more=True",
                "token file present: True",
                "sign-in in use: Apple's native sign-in",
            ]
        );
        assert_eq!((ran.status, ran.err.as_str()), (0, ""));
        // two one-item pages, with the sign-in's token, which is printed nowhere
        let asked: Vec<String> = scratch.wire.lock().unwrap().iter().map(|request| request.url.clone()).collect();
        let pages =
            ["playlists", "songs"].map(|kind| format!("https://amp-api.music.apple.com/v1/me/library/{kind}?limit=1"));
        assert_eq!(asked, pages);
        assert!(scratch.wire.lock().unwrap().iter().all(|request| request.token().as_deref() == Some(ANDROID_TOKEN)));
        assert!(!ran.out.contains(ANDROID_TOKEN));
        // the state is kept: `status` prints it without asking
        let offline = scratch.run(&["status"]);
        assert_eq!((offline.status, offline.out.as_str()), (0, ran.out.as_str()));
        assert_eq!(scratch.wire.lock().unwrap().len(), 2);

        // Apple rejects the sign-in
        *scratch.verdict.lock().unwrap() = 401;
        let ran = scratch.run(&["check"]);
        assert_eq!(
            lines_without_the_time(&ran.out),
            [
                "state: reconnect_required",
                "checked_at: <now>",
                "http_status: 401",
                "token file present: True",
                "sign-in in use: Apple's native sign-in",
            ]
        );
        assert_eq!((ran.status, ran.err.as_str()), (1, format!("{RECONNECT_HINT}\n").as_str()));

        // a check that cannot be made says why and changes nothing
        *scratch.verdict.lock().unwrap() = 500;
        let ran = scratch.run(&["check"]);
        assert_eq!((ran.status, ran.out.as_str()), (1, ""));
        assert_eq!(ran.err, "Error: amp-api.music.apple.com returned HTTP 500\n");
        assert_eq!(scratch.run(&["status"]).out.lines().next(), Some("state: reconnect_required"));
    }

    #[test]
    fn check_of_a_saved_browser_session() {
        let scratch = Scratch::new();
        sign_in(&scratch.config, Source::Browser, BROWSER_TOKEN);
        *scratch.verdict.lock().unwrap() = 200;
        let ran = scratch.run(&["check"]);
        assert_eq!(ran.status, 0);
        let lines = lines_without_the_time(&ran.out);
        assert_eq!(
            (lines[0].as_str(), lines.last().unwrap().as_str()),
            ("state: connected", "sign-in in use: browser session")
        );
        assert!(!ran.out.contains(BROWSER_TOKEN));
    }

    #[test]
    fn connect_is_the_native_sign_in() {
        let mut scratch = Scratch::new();
        // the browser sign-in is not part of this engine: a message, and nobody is signed in
        for args in [&["connect", "--browser"][..], &["connect", "--timeout", "60", "--browser"]] {
            let ran = scratch.run(args);
            assert_eq!((ran.status, ran.out.as_str()), (2, ""), "{args:?}");
            assert_eq!(ran.err, format!("{NO_BROWSER_SIGNIN}\n"));
        }
        assert!(NO_BROWSER_SIGNIN.contains("no browser sign-in"));

        // where the sign-in cannot run
        for (error, said) in [
            (
                SessionError::NotBuilt,
                "Error: The native sign-in needs the lossless route, which is not built on this machine (./setup.sh \
                 builds it on x86_64)\n",
            ),
            (
                SessionError::NoTerminal,
                "Error: The native sign-in asks for the password and the verification code in a terminal: run this \
                 in one, or use the Sign In button of the app\n",
            ),
            (
                SessionError::HelperStart("NotFound".to_string()),
                "Error: Could not start the sign-in helper (NotFound)\n",
            ),
        ] {
            scratch.context.signin = Box::new(move |_| Err(error.clone()));
            let ran = scratch.run(&["connect"]);
            assert_eq!((ran.status, ran.out.as_str(), ran.err.as_str()), (1, "", said));
        }

        // a sign-in that was not completed
        scratch.context.signin = Box::new(|_| Ok(false));
        let ran = scratch.run(&["connect"]);
        assert_eq!((ran.status, ran.out.as_str()), (1, ""));
        assert_eq!(ran.err, "Sign-in not completed; an earlier sign-in on this machine is kept.\n");
        assert!(scratch.wire.lock().unwrap().is_empty());

        // a completed one is checked, and its state printed
        let (config, attempts) = (scratch.config.clone(), Arc::new(AtomicUsize::new(0)));
        let counted = attempts.clone();
        scratch.context.signin = Box::new(move |web| {
            counted.fetch_add(1, Ordering::SeqCst);
            sign_in(&config, Source::Android, ANDROID_TOKEN); // what the helper leaves
            web.reset_token_sources();
            Ok(true)
        });
        *scratch.verdict.lock().unwrap() = 200;
        for args in [&["connect"][..], &["connect", "--timeout", "600"]] {
            let ran = scratch.run(args);
            let lines = lines_without_the_time(&ran.out);
            let (first, last) = (lines[0].as_str(), lines.last().unwrap().as_str());
            assert_eq!((first, last), ("state: connected", "sign-in in use: Apple's native sign-in"));
            assert_eq!((ran.status, ran.err.as_str()), (0, ""), "{args:?}");
            assert!(!ran.out.contains(ANDROID_TOKEN));
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        // signed in, and Apple does not accept it
        *scratch.verdict.lock().unwrap() = 403;
        let ran = scratch.run(&["connect"]);
        assert_eq!((ran.status, ran.out.lines().next()), (1, Some("state: reconnect_required")));

        // usage
        let ran = scratch.run(&["connect", "--timeout", "soon"]);
        let usage = "usage: amui connect [-h] [--browser] [--timeout TIMEOUT]\n";
        assert_eq!(ran.err, format!("{usage}amui connect: error: argument --timeout: invalid int value: 'soon'\n"));
        assert_eq!((ran.status, attempts.load(Ordering::SeqCst)), (2, 3));
    }

    #[test]
    fn usage_errors_ask_nobody() {
        let scratch = Scratch::with(&[], reference_network());
        for (args, usage, error) in [
            (
                &["albums", "--limit", "x"][..],
                "usage: amui albums [-h] [--offset OFFSET] [--limit LIMIT]",
                "amui albums: error: argument --limit: invalid int value: 'x'",
            ),
            (
                &["recently-added", "--offset"],
                "usage: amui recently-added [-h] [--offset OFFSET] [--limit LIMIT]",
                "amui recently-added: error: argument --offset: expected one argument",
            ),
            (
                &["tracks", "--limit", "5"],
                "usage: amui tracks [-h] [--offset OFFSET] [--limit LIMIT] key",
                "amui tracks: error: the following arguments are required: key",
            ),
            (
                &["search", "--limit", "3"],
                "usage: amui search [-h] [--limit LIMIT] term [term ...]",
                "amui search: error: the following arguments are required: term",
            ),
            (
                &["lyrics", "1000000001", "1000000002", "--show"],
                "usage: amui lyrics [-h] [--show] catalog_id",
                "amui lyrics: error: unrecognized arguments: 1000000002",
            ),
            (
                &["song"],
                "usage: amui song [-h] catalog_id",
                "amui song: error: the following arguments are required: catalog_id",
            ),
            (&["status", "now"], "usage: amui status [-h]", "amui status: error: unrecognized arguments: now"),
            (&["home", "--limit", "3"], "usage: amui home [-h]", "amui home: error: unrecognized arguments: --limit 3"),
        ] {
            let ran = scratch.run(args);
            assert_eq!((ran.status, ran.out.as_str()), (2, ""), "{args:?}");
            assert_eq!(ran.err, format!("{usage}\n{error}\n"), "{args:?}");
        }
        assert!(scratch.asked().is_empty());
        // options may be shortened and written with `=`, as argparse took them
        let ran = scratch.run(&["albums", "--off=100", "--lim", "2"]);
        assert_eq!((ran.status, ran.out.lines().last()), (0, Some("1 albums of 257")));
        assert_eq!(scratch.asked(), ["https://amp-api.music.apple.com/v1/me/library/albums?limit=2&offset=100"]);
    }

    #[test]
    fn a_station_has_a_row_too() {
        let station = Station {
            id: "ra.1000000001".into(),
            name: "Station A".into(),
            subtitle: "Station".into(),
            ..Station::default()
        };
        assert_eq!(Item::Station(station).row(), ["ra.1000000001", "Station A", "Station", ""]);
    }

    #[test]
    fn lengths_and_times() {
        assert_eq!(hms(None), "   --   ");
        assert_eq!(hms(Some(0.0)), "00:00.000");
        assert_eq!(hms(Some(125.5)), "02:05.500");
        assert_eq!(hms(Some(-1.0)), "00:00.000");
        assert_eq!(hms(Some(f64::NAN)), "00:00.000");
        assert_eq!([mmss(None), mmss(Some(201_000)), mmss(Some(59_999))], ["", "3:21", "0:59"]);
        assert_eq!(clip("a much longer text", 6), "a muc…");
        assert_eq!(table(&["a"], &[], &[]), ["  (none)"]);
        assert_eq!(
            table(&["a", "bb"], &[vec!["xyz".to_string(), "1".to_string()]], &[2, 9]),
            ["a   bb", "--  --", "x…  1 "]
        );
    }
}
