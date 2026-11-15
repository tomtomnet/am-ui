//! Apple Music sign-in: status, live check, and the native sign-in.
//!
//! Two sign-ins give the same credential (the account's music user token; see
//! [`Web::token_sources`]):
//!
//! * "android": Apple's native sign-in, run by the wrapper of the lossless route
//!   (tools/lossless). It is what Connect does wherever that route is built, and the one that
//!   also unlocks lossless (ALAC). The application asks for the Apple ID, the password and the
//!   six-digit code in its own sheet and hands them to the wrapper through [`AndroidSignIn`];
//!   nothing is stored or logged on the way.
//! * "browser": Apple's web sign-in in a dedicated Chromium window, which the Python application
//!   runs through a workspace helper. This engine does not run it. A browser session that is
//!   already saved keeps working: requests use it (`web`), [`status`] reports it and [`check`]
//!   verifies it. Lossless stays unavailable with it.
//!
//! * [`status`]: offline: state of the sign-in in use, which one it is, and which route Connect
//!   takes here.
//! * [`check`]: live read-only verification (two one-item library pages); 401/403 ->
//!   "reconnect_required"; when Apple rejects the sign-in in use and accepts the other one, that
//!   one takes over.
//! * [`signin_route`]: "android" or "none": what Connect does on this machine.
//! * [`AndroidSignIn`]: a running native sign-in (`wrapperctl.py login --pipe`), started by
//!   [`android_signin`].
//! * [`android_signin_terminal`]: the same sign-in in the terminal this process runs in.
//!
//! No token value passes through this module: the checks go through `web` with one sign-in
//! pinned, and the status files hold states and times only. What is typed into a sign-in (Apple
//! ID, password, verification code) goes to the helper over its standard input and nowhere else:
//! not into its arguments or its environment, a file, a log, an error or `Debug` output.
//!
//! Differences from amui/session.py:
//!
//! * No browser sign-in: `connect()`, `ConnectProcess`, `desktop_available()` and the bounds of
//!   its timeout are not here. [`signin_route`] therefore answers "android" where the native
//!   sign-in can run and "none" otherwise, whatever `AMUI_SIGNIN` says (that variable still puts
//!   a saved browser session first for requests), and the message for a machine without the
//!   lossless route no longer ends with "use the browser sign-in instead".
//! * The check of a saved browser session is made here, through `web` like the native one,
//!   instead of by the workspace's `tools/music_session.py`: the same two requests and the same
//!   status file, written the same way. When it cannot be made, the error carries `web`'s text
//!   (host and status) where the Python module passed on the workspace probe's.
//! * Everything takes the [`Web`] (or [`Config`]) it works on instead of module state, and the
//!   answers are typed: [`Status`], [`SignInEvent`], [`SignInResult`], whose `to_json()` gives
//!   the dictionaries of the Python module. A value of an unexpected type in a status file or
//!   in a line of the helper is left out, not passed on. An [`AndroidSignIn`] comes from
//!   [`android_signin`] only; the Python class could be put around any process.
//! * "Could not start the sign-in helper (NotFound)" names the kind of the system's error, as
//!   `web` does, where Python named its exception class.
//! * The lines for the helper carry text outside ASCII as UTF-8 where Python wrote `\u` escapes;
//!   the helper reads both as the same JSON. A line of the helper longer than a mebibyte is not
//!   looked at (its own are a few hundred bytes; Python read lines of any length).
//! * The helper is started without what Python's `close_fds` did: descriptors of this process
//!   that are not close-on-exec go along. The engine opens its own close-on-exec.

use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::config::{self, Config};
use crate::util::{self, Event};
use crate::web::{Http, Source, Web, WebError};

/// The states [`status`] and [`check`] report ([`State`]).
pub const STATES: [&str; 4] = ["connected", "reconnect_required", "no_session", "unknown"];
/// The only keys ever read out of a status file or written to one: non-secret metadata.
const SAFE_KEYS: [&str; 7] =
    ["state", "checked_at", "cookie_expires_at", "library", "captured_at", "source", "http_status"];
/// Where a native sign-in can be ([`Phase`]).
pub const SIGNIN_PHASES: [&str; 5] = ["preparing", "signing_in", "code_required", "verifying", "finishing"];

/// How long a cancelled helper has to end by itself, and again after it was told to terminate.
const CANCEL_GRACE: Duration = Duration::from_secs(20);
/// The longest line of the helper that is looked at (its own are a few hundred bytes). A longer
/// one is read in pieces of this size and left out.
const EVENT_LINE_LIMIT: u64 = 1 << 20;
const UNEXPECTED_LIBRARY: &str = "Apple did not return the expected library response";
const HELPER_STOPPED: &str = "The sign-in helper stopped unexpectedly";

macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$about:meta])* $variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$about])* $variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(&self) -> &'static str {
                match self { $($name::$variant => $text),+ }
            }

            pub fn parse(text: &str) -> Option<$name> {
                match text { $($text => Some($name::$variant),)+ _ => None }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

text_enum!(
    /// The state of the sign-in in use, one of [`STATES`].
    State {
        /// Apple accepted it at the last check.
        Connected => "connected",
        /// Apple rejected it (401/403): sign in again.
        ReconnectRequired => "reconnect_required",
        /// This machine holds no sign-in.
        NoSession => "no_session",
        /// There is a sign-in; no check has said yet whether Apple accepts it.
        Unknown => "unknown",
    }
);

text_enum!(
    /// What Connect does on this machine.
    Route {
        /// Apple's native sign-in.
        Android => "android",
        /// Nothing: the native sign-in cannot run here and there is no other.
        None => "none",
    }
);

text_enum!(
    /// Where a native sign-in is, one of [`SIGNIN_PHASES`].
    Phase {
        Preparing => "preparing",
        SigningIn => "signing_in",
        /// Apple asked for the six-digit code: [`AndroidSignIn::submit_code`].
        CodeRequired => "code_required",
        Verifying => "verifying",
        Finishing => "finishing",
    }
);

/// Session workflow problem with a message that is safe to show and log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// The native sign-in cannot run on this machine.
    #[error(
        "The native sign-in needs the lossless route, which is not built on this machine \
         (./setup.sh builds it on x86_64)"
    )]
    NotBuilt,
    /// [`android_signin_terminal`] outside a terminal.
    #[error(
        "The native sign-in asks for the password and the verification code in a terminal: \
         run this in one, or use the Sign In button of the app"
    )]
    NoTerminal,
    /// The sign-in helper did not start; carries the kind of the system's error.
    #[error("Could not start the sign-in helper ({0})")]
    HelperStart(String),
    /// A live check could not be made or its outcome not be kept: host and status, no more.
    #[error("{0}")]
    Check(String),
}

// --- status files --------------------------------------------------------------

fn status_file(config: &Config, source: Source) -> &Path {
    match source {
        Source::Android => &config.android_session_status_file,
        Source::Browser => &config.session_status_file,
    }
}

/// What a status file holds of [`SAFE_KEYS`]: non-secret metadata only. Empty when the file is
/// missing, a link, unreadable or not a JSON object.
fn read_status_file(config: &Config, source: Source) -> Map<String, Value> {
    let path = status_file(config, source);
    // a regular file, not a link to one
    if !fs::symlink_metadata(path).is_ok_and(|info| info.file_type().is_file()) {
        return Map::new();
    }
    let data = util::read_json_object(path).unwrap_or_default();
    SAFE_KEYS.iter().filter_map(|key| data.get(*key).map(|value| (key.to_string(), value.clone()))).collect()
}

/// A status record as the Python module writes it (`json.dump(record, indent=2)` and a newline):
/// the keys of [`SAFE_KEYS`] only, in the order given, characters outside ASCII as `\u` escapes.
fn status_text(record: &Map<String, Value>) -> String {
    let safe: Map<String, Value> = record
        .iter()
        .filter(|(key, _)| SAFE_KEYS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let mut text = String::new();
    // outside ASCII there is only text inside strings, where an escape says the same
    for character in format!("{:#}\n", Value::Object(safe)).chars() {
        if character.is_ascii() {
            text.push(character);
        } else {
            for unit in character.encode_utf16(&mut [0; 2]) {
                text.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    text
}

/// Two checks at once in this process write one after the other.
static STATUS_WRITES: Mutex<()> = Mutex::new(());

fn write_status_file(path: &Path, record: &Map<String, Value>) -> io::Result<()> {
    let text = status_text(record);
    let _one_at_a_time = STATUS_WRITES.lock().unwrap();
    util::write_private_atomic(path, text.as_bytes())
}

/// State and time of the last check of the native sign-in (`None`: forget it). No secrets.
fn write_android_status(config: &Config, record: Option<&Map<String, Value>>) {
    let path = &config.android_session_status_file;
    // a failure is not reported: the next check simply runs again
    let Some(record) = record else {
        let _ = fs::remove_file(path);
        return;
    };
    if config.ensure_dirs().is_ok() {
        let _ = write_status_file(path, record);
    }
}

/// The last check of the saved browser session, next to its token: the folder is made private
/// on every write.
fn write_browser_status(config: &Config, record: &Map<String, Value>) -> io::Result<()> {
    let path = &config.session_status_file;
    if let Some(folder) = path.parent() {
        util::create_private_dirs(folder)?;
        fs::set_permissions(folder, fs::Permissions::from_mode(0o700))?;
    }
    write_status_file(path, record)
}

/// The time as Python's `datetime.now(timezone.utc).isoformat()` writes it:
/// "2026-01-01T00:00:00.123456+00:00", without the fraction when it is zero.
fn utc_isoformat(since_epoch: Duration) -> String {
    let (days, rest) = (since_epoch.as_secs() / 86_400, since_epoch.as_secs() % 86_400);
    // days since 1970-01-01 to a date of the Gregorian calendar, in years that start in March
    let shifted = days + 719_468;
    let (era, day_of_era) = (shifted / 146_097, shifted % 146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * march_month + 2) / 5 + 1;
    let month = if march_month < 10 { march_month + 3 } else { march_month - 9 };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    let fraction = match since_epoch.subsec_micros() {
        0 => String::new(),
        micros => format!(".{micros:06}"),
    };
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}{fraction}+00:00", rest / 3600, rest % 3600 / 60, rest % 60)
}

fn now() -> String {
    utc_isoformat(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default())
}

// --- status --------------------------------------------------------------------

/// True when the native sign-in can run here: the lossless route is built and its runtime staged
/// (`./setup.sh` does both on x86_64 unless `--no-lossless`).
pub fn android_signin_available(config: &Config) -> bool {
    config.native_signin_built()
}

/// What Connect does on this machine: "android" (the native sign-in) wherever it can run, "none"
/// elsewhere. File checks only.
pub fn signin_route(config: &Config) -> Route {
    if android_signin_available(config) { Route::Android } else { Route::None }
}

/// What [`status`] and [`check`] say about the sign-in in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    /// When the sign-in in use was last checked (ISO 8601, UTC).
    pub checked_at: Option<String>,
    /// Browser session: until when its cookie is valid.
    pub cookie_expires_at: Option<String>,
    /// What the last check saw: `{"playlists": {"accessible": true, "sample_count": 1,
    /// "has_more": true}, "songs": {...}}`. Counts only, no names or ids.
    pub library: Option<Map<String, Value>>,
    /// Browser session: when it was signed in.
    pub captured_at: Option<String>,
    /// The HTTP status of Apple's rejection.
    pub http_status: Option<u16>,
    /// This machine holds a sign-in.
    pub has_token: bool,
    /// The sign-in in use.
    pub signin: Option<Source>,
    /// What Connect does here.
    pub signin_route: Route,
}

impl Status {
    /// The dictionary `status()` of the Python module returns.
    pub fn to_json(&self) -> Value {
        json!({
            "state": self.state.as_str(),
            "checked_at": self.checked_at,
            "cookie_expires_at": self.cookie_expires_at,
            "library": self.library,
            "captured_at": self.captured_at,
            "http_status": self.http_status,
            "has_token": self.has_token,
            "signin": self.signin.map(|source| source.as_str()),
            "signin_route": self.signin_route.as_str(),
        })
    }
}

/// Cheap, offline: the state of the sign-in in use from its status file plus token-file
/// presence, which sign-in that is, and what Connect does here.
pub fn status(web: &Web) -> Status {
    let config = web.config();
    let source = web.token_source();
    let data = source.map(|source| read_status_file(config, source)).unwrap_or_default();
    let state = match (source, data.get("state").and_then(Value::as_str)) {
        (None, _) => State::NoSession,
        (Some(_), Some("connected")) => State::Connected,
        (Some(_), Some("reconnect_required")) => State::ReconnectRequired,
        (Some(_), _) => State::Unknown,
    };
    let text = |key: &str| data.get(key).and_then(Value::as_str).map(str::to_owned);
    Status {
        state,
        checked_at: text("checked_at"),
        cookie_expires_at: text("cookie_expires_at"),
        library: data.get("library").and_then(Value::as_object).cloned(),
        captured_at: text("captured_at"),
        http_status: data.get("http_status").and_then(Value::as_u64).and_then(|status| u16::try_from(status).ok()),
        has_token: source.is_some(),
        signin: source,
        signin_route: signin_route(config),
    }
}

// --- live check ----------------------------------------------------------------

/// What Python's `bool()` says of a JSON value.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// Why the two library pages of a check were not read.
enum Refused {
    /// Apple's API rejected the sign-in (with the HTTP status), or its file cannot be used
    /// (without one); what `web` says about it.
    SignIn(Option<u16>, String),
    /// The check could not be made, or its answer was no library page.
    Check(String),
}

impl From<WebError> for Refused {
    fn from(error: WebError) -> Refused {
        let said = error.to_string(); // host + status only
        match error {
            // `web` reports a 401/403 of any host this way, also of music.apple.com, which it
            // asks without credentials for Apple's public developer token: a refusal there says
            // nothing about the sign-in. A rejection by the API names the API's host first.
            WebError::Auth { status: Some(_), .. } if !said.starts_with(config::API_HOST) => Refused::Check(said),
            WebError::Auth { status, .. } => Refused::SignIn(status, said),
            _ => Refused::Check(said),
        }
    }
}

/// What the two one-item library pages say when asked with the credentials of one sign-in only
/// (no second attempt with the other one). Counts are kept, no names and no ids.
fn library_access(web: &Web, source: Source) -> Result<Map<String, Value>, Refused> {
    let mut access = Map::new();
    for kind in ["playlists", "songs"] {
        let url = format!("https://{}/v1/me/library/{kind}", config::API_HOST);
        let body = web.get_json_with(&url, &[("limit", "1")], true, 20.0, Some(source))?;
        let Some(Value::Array(items)) = body.get("data") else {
            return Err(Refused::Check(UNEXPECTED_LIBRARY.to_string()));
        };
        let has_more = body.get("next").is_some_and(truthy);
        access.insert(kind.to_string(), json!({"accessible": true, "sample_count": items.len(), "has_more": has_more}));
    }
    Ok(access)
}

fn connected(access: Map<String, Value>) -> Map<String, Value> {
    Map::from_iter([
        ("state".to_string(), json!("connected")),
        ("checked_at".to_string(), json!(now())),
        ("library".to_string(), Value::Object(access)),
    ])
}

fn rejected(status: Option<u16>) -> Map<String, Value> {
    let mut record =
        Map::from_iter([("state".to_string(), json!("reconnect_required")), ("checked_at".to_string(), json!(now()))]);
    if let Some(status) = status {
        record.insert("http_status".to_string(), json!(status));
    }
    record
}

/// Two one-item library pages with the native sign-in's credentials only.
fn check_android(web: &Web) -> Result<State, SessionError> {
    let config = web.config();
    match library_access(web, Source::Android) {
        Ok(access) => {
            write_android_status(config, Some(&connected(access)));
            Ok(State::Connected)
        }
        // Apple rejected it, or its file is not usable: either way it takes a new sign-in
        Err(Refused::SignIn(status, _)) => {
            write_android_status(config, Some(&rejected(status)));
            Ok(State::ReconnectRequired)
        }
        Err(Refused::Check(problem)) => Err(SessionError::Check(problem)),
    }
}

/// The same two pages with the saved browser session's cookie only; the outcome goes into the
/// status file next to it, keeping what the sign-in recorded there about itself.
fn check_browser(web: &Web) -> Result<State, SessionError> {
    let config = web.config();
    let record = match library_access(web, Source::Browser) {
        Ok(access) => {
            let mut record = connected(access);
            let old = read_status_file(config, Source::Browser);
            for key in ["captured_at", "cookie_expires_at", "source"] {
                if let Some(value) = old.get(key) {
                    record.insert(key.to_string(), value.clone());
                }
            }
            record
        }
        Err(Refused::SignIn(Some(status), _)) => rejected(Some(status)),
        // a session file that cannot be used is a problem to name, not an answer of Apple's
        Err(Refused::SignIn(None, problem) | Refused::Check(problem)) => return Err(SessionError::Check(problem)),
    };
    write_browser_status(config, &record)
        .map_err(|error| SessionError::Check(format!("Session check failed ({:?})", error.kind())))?;
    let saved = read_status_file(config, Source::Browser);
    Ok(saved.get("state").and_then(Value::as_str).and_then(State::parse).unwrap_or(State::Unknown))
}

/// Live read-only verification (two one-item library pages) of the sign-in in use; returns
/// [`status`] afterwards. 401/403 becomes state "reconnect_required" without discarding
/// anything. When Apple rejects the sign-in in use and this machine holds the other one, that
/// one is verified too and takes over if Apple accepts it.
pub fn check(web: &Web) -> Result<Status, SessionError> {
    check_with(web, check_browser)
}

/// [`check`] with the check of the browser session given by the caller.
fn check_with(web: &Web, browser: impl Fn(&Web) -> Result<State, SessionError>) -> Result<Status, SessionError> {
    let sources = web.token_sources();
    for (index, source) in sources.iter().enumerate() {
        let state = match source {
            Source::Android => check_android(web)?,
            Source::Browser => browser(web)?,
        };
        if state == State::Connected {
            for earlier in &sources[..index] {
                web.demote_source(*earlier);
            }
            break;
        }
        if state != State::ReconnectRequired {
            break;
        }
    }
    Ok(status(web))
}

// --- the native sign-in ----------------------------------------------------------

/// How a native sign-in ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInResult {
    /// A fresh sign-in exists; requests use it from now on.
    pub ok: bool,
    /// Why not: "input", "unavailable", "busy", "credentials", "code", "apple", "cancelled",
    /// "timeout" or "failed".
    pub kind: Option<String>,
    /// The reason in words, safe to show.
    pub error: Option<String>,
    /// How far the sign-in got, by marker.
    pub steps: Vec<String>,
    /// The file with the sign-in component's last lines after a failed attempt (account name,
    /// typed secrets and tokens left out).
    pub log: Option<String>,
}

/// A value of a helper line as text: text as it is, anything else the way JSON writes it.
fn shown(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn clipped(text: &str, characters: usize) -> String {
    text.chars().take(characters).collect()
}

impl SignInResult {
    /// The result line of the helper, with bounds on what it can make this application show.
    fn from_event(event: &Map<String, Value>) -> SignInResult {
        let error = event.get("error").filter(|error| truthy(error));
        let steps = match event.get("steps") {
            Some(Value::Array(steps)) => steps.iter().take(40).map(|step| clipped(&shown(step), 40)).collect(),
            _ => Vec::new(),
        };
        SignInResult {
            ok: event.get("ok").is_some_and(truthy),
            kind: event.get("kind").and_then(Value::as_str).map(str::to_owned),
            error: error.map(|error| clipped(&shown(error), 600)),
            steps,
            log: event.get("log").and_then(Value::as_str).map(str::to_owned),
        }
    }

    /// The helper ended without saying how.
    fn stopped() -> SignInResult {
        SignInResult {
            ok: false,
            kind: Some("failed".to_string()),
            error: Some(HELPER_STOPPED.to_string()),
            steps: Vec::new(),
            log: None,
        }
    }

    /// The dictionary `AndroidSignIn.result()` of the Python module returns.
    pub fn to_json(&self) -> Value {
        json!({"ok": self.ok, "kind": self.kind, "error": self.error, "steps": self.steps, "log": self.log})
    }
}

/// What a running sign-in tells its listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInEvent {
    /// `{"event": "phase", "phase": ...}`: where the sign-in is now.
    Phase(Phase),
    /// `{"event": "notice", "error": ...}`: the helper did not take an input line (the code was
    /// not six digits); the sign-in goes on.
    Notice { error: String },
    /// `{"event": "result", ...}`: the end, once the helper has exited and cleaned up.
    Result(SignInResult),
}

impl SignInEvent {
    /// The dictionary a listener of the Python module receives.
    pub fn to_json(&self) -> Value {
        match self {
            SignInEvent::Phase(phase) => json!({"event": "phase", "phase": phase.as_str()}),
            SignInEvent::Notice { error } => json!({"event": "notice", "error": error}),
            SignInEvent::Result(result) => {
                let mut event = Map::from_iter([("event".to_string(), json!("result"))]);
                if let Value::Object(result) = result.to_json() {
                    event.extend(result);
                }
                Value::Object(event)
            }
        }
    }
}

/// Receives the events of a sign-in, on its reader thread.
pub type OnEvent = Box<dyn FnMut(SignInEvent) + Send>;

fn deliver(on_event: &mut Option<OnEvent>, event: SignInEvent) {
    if let Some(listener) = on_event {
        // a listener problem must not stop the sign-in
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(event)));
    }
}

/// One line for the helper: a JSON object of text values, spelled as Python's `json.dumps`
/// spells it. What was typed is written into this one buffer and nowhere else.
fn json_line(pairs: &[(&str, &str)]) -> Vec<u8> {
    // the longest a value can get is six bytes for each of its own, so the buffer never moves
    let room: usize = pairs.iter().map(|(key, value)| 6 * (key.len() + value.len()) + 8).sum();
    let mut line = Vec::with_capacity(room + 2);
    line.push(b'{');
    for (index, (key, value)) in pairs.iter().enumerate() {
        if index > 0 {
            line.extend_from_slice(b", ");
        }
        // text into a buffer in memory: neither step can fail
        let _ = serde_json::to_writer(&mut line, key);
        line.extend_from_slice(b": ");
        let _ = serde_json::to_writer(&mut line, value);
    }
    line.extend_from_slice(b"}\n");
    line
}

/// What a running sign-in consists of, shared by its handle, its reader and a cancel.
struct Shared {
    /// The helper. The reader waits for it; a cancel may have to stop it.
    process: Mutex<Child>,
    pid: u32,
    /// The pipe to the helper's standard input; `None` once the sign-in has ended.
    stdin: Mutex<Option<ChildStdin>>,
    phase: Mutex<Phase>,
    result: Mutex<Option<SignInResult>>,
    /// Set when the reader is through: the helper has exited and its result was delivered.
    finished: Event,
    grace: Duration,
}

impl Shared {
    /// Write one line to the helper. The buffer is cleared afterwards: what was typed into the
    /// sign-in does not stay behind in memory this module has given back.
    fn send(&self, mut line: Vec<u8>) -> bool {
        let sent = match self.stdin.lock().unwrap().as_mut() {
            Some(stdin) => stdin.write_all(&line).and_then(|()| stdin.flush()).is_ok(),
            None => false, // the helper has ended; its result says why
        };
        line.fill(0);
        std::hint::black_box(&mut line); // or the compiler may drop a write to memory about to be freed
        sent
    }

    /// The reader: the helper's lines until it closes its output, then its end.
    fn pump(&self, stdout: Option<ChildStdout>, web: &Web, mut on_event: Option<OnEvent>) {
        let mut result = None;
        if let Some(stdout) = stdout {
            let mut lines = BufReader::new(stdout);
            let mut raw = Vec::new();
            let mut unfinished = false; // the line of the piece before goes on
            loop {
                raw.clear();
                if !matches!((&mut lines).take(EVENT_LINE_LIMIT).read_until(b'\n', &mut raw), Ok(read) if read > 0) {
                    break;
                }
                let rest_of_a_long_line = unfinished;
                unfinished = raw.len() as u64 == EVENT_LINE_LIMIT && raw.last() != Some(&b'\n');
                if rest_of_a_long_line || unfinished {
                    continue;
                }
                let Ok(Value::Object(event)) = serde_json::from_slice(&raw) else { continue };
                match event.get("event").and_then(Value::as_str) {
                    Some("phase") => {
                        let Some(phase) = event.get("phase").and_then(Value::as_str).and_then(Phase::parse) else {
                            continue;
                        };
                        *self.phase.lock().unwrap() = phase;
                        deliver(&mut on_event, SignInEvent::Phase(phase));
                    }
                    // delivered below, once the helper has exited and cleaned up
                    Some("result") => result = Some(SignInResult::from_event(&event)),
                    Some("notice") => {
                        let error = event.get("error").filter(|error| truthy(error)).map(shown).unwrap_or_default();
                        deliver(&mut on_event, SignInEvent::Notice { error });
                    }
                    _ => {}
                }
            }
        }
        self.wait_ended(None);
        let result = result.unwrap_or_else(SignInResult::stopped);
        if result.ok {
            web.reset_token_sources(); // the new sign-in is the one to use
            write_android_status(web.config(), None); // not checked yet
        }
        self.stdin.lock().unwrap().take();
        *self.result.lock().unwrap() = Some(result.clone());
        deliver(&mut on_event, SignInEvent::Result(result));
        self.finished.set();
    }

    /// Whether the helper has ended (it has been waited for then).
    fn ended(&self) -> bool {
        // an error: it cannot be waited for, so there is nothing left to wait for or to stop
        !matches!(self.process.lock().unwrap().try_wait(), Ok(None))
    }

    /// Wait for the helper to end, at most `limit`; whether it has.
    fn wait_ended(&self, limit: Option<Duration>) -> bool {
        let deadline = limit.map(|limit| Instant::now() + limit);
        let mut pause = Duration::from_millis(1);
        loop {
            if self.ended() {
                return true;
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return false;
            }
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(50));
        }
    }

    /// After a cancel: give the helper time to end, then make it.
    fn ensure_ended(&self) {
        if self.wait_ended(Some(self.grace)) {
            return;
        }
        self.terminate(); // the helper turns this into the same clean cancel
        if !self.wait_ended(Some(self.grace)) {
            self.kill();
        }
    }

    fn terminate(&self) {
        let mut process = self.process.lock().unwrap();
        // Only while it has not been waited for: afterwards its id may be another process's.
        // The lock keeps the reader from waiting for it in between.
        if let (Ok(None), Ok(pid)) = (process.try_wait(), libc::pid_t::try_from(self.pid))
            && pid > 0
        {
            // SAFETY: a signal to this process's own child, which still exists (see above).
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }

    fn kill(&self) {
        // the standard library sends nothing to a child that has been waited for
        let _ = self.process.lock().unwrap().kill();
    }
}

/// A running native sign-in: `wrapperctl.py login --pipe --stop-service`, which runs Apple's own
/// Android sign-in in the wrapper of the lossless route and reports where it is.
///
/// The Apple ID and password go to the helper once, over its stdin pipe, and are not kept here;
/// the six-digit code follows the same way through [`submit_code`](AndroidSignIn::submit_code).
/// Nothing of that reaches a command line, a log or a file of this application. The listener
/// (called on a reader thread) receives [`SignInEvent::Phase`], [`SignInEvent::Notice`] and
/// finally [`SignInEvent::Result`], whose `steps` say how far the sign-in got and whose `log` is
/// the file with the sign-in component's last lines after a failed attempt.
///
/// Dropping the handle does not stop the sign-in; [`cancel`](AndroidSignIn::cancel) does.
pub struct AndroidSignIn {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for AndroidSignIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AndroidSignIn")
            .field("pid", &self.shared.pid)
            .field("phase", &self.phase())
            .field("running", &self.running())
            .finish()
    }
}

impl AndroidSignIn {
    /// Follow a helper that was started with pipes for its standard input and output. `grace` is
    /// what a cancel gives it to end by itself.
    fn follow(
        mut process: Child,
        web: Arc<Web>,
        on_event: Option<OnEvent>,
        grace: Duration,
    ) -> Result<AndroidSignIn, SessionError> {
        let stdout = process.stdout.take();
        let shared = Arc::new(Shared {
            pid: process.id(),
            stdin: Mutex::new(process.stdin.take()),
            process: Mutex::new(process),
            phase: Mutex::new(Phase::Preparing),
            result: Mutex::new(None),
            finished: Event::new(),
            grace,
        });
        let reader = shared.clone();
        let started =
            std::thread::Builder::new().name("amui-signin".into()).spawn(move || reader.pump(stdout, &web, on_event));
        if let Err(error) = started {
            // nobody would read what the helper says: closing its input cancels the sign-in
            shared.stdin.lock().unwrap().take();
            return Err(SessionError::HelperStart(format!("{:?}", error.kind())));
        }
        Ok(AndroidSignIn { shared })
    }

    /// Where the sign-in is.
    pub fn phase(&self) -> Phase {
        *self.shared.phase.lock().unwrap()
    }

    /// Send the six-digit verification code (only meaningful in phase "code_required"). False
    /// when the helper no longer listens.
    pub fn submit_code(&self, code: &str) -> bool {
        self.shared.send(json_line(&[("code", code.trim())]))
    }

    /// Stop the sign-in (returns at once); an earlier sign-in on this machine is kept. The helper
    /// also cancels by itself when this process goes away.
    pub fn cancel(&self) {
        if !self.running() {
            return;
        }
        self.shared.send(b"{\"cancel\": true}\n".to_vec());
        let watchdog = self.shared.clone();
        let _ = std::thread::Builder::new().name("amui-signin-cancel".into()).spawn(move || watchdog.ensure_ended());
    }

    pub fn running(&self) -> bool {
        !self.shared.finished.is_set()
    }

    /// Block until the sign-in has ended (or `timeout` passes); returns [`result`](AndroidSignIn::result).
    pub fn wait(&self, timeout: Option<Duration>) -> Option<SignInResult> {
        self.shared.finished.wait(timeout);
        self.result()
    }

    /// `None` while running; afterwards how the sign-in ended.
    pub fn result(&self) -> Option<SignInResult> {
        if self.running() { None } else { self.shared.result.lock().unwrap().clone() }
    }
}

/// The sign-in helper of the lossless route with these arguments, as `python3 -I wrapperctl.py
/// ...`. It finds its data folder and its port the way [`Config::from_process`] does, from where
/// it lies and from the environment it inherits.
fn helper_command(config: &Config, arguments: &[&str]) -> Result<Command, SessionError> {
    if !android_signin_available(config) {
        return Err(SessionError::NotBuilt);
    }
    Ok(config.helper_command(arguments))
}

/// Start a helper that creates private files only (umask 077). `own_session` also takes it out
/// of this process's session, so nothing typed at a terminal of this process reaches it.
fn start(mut command: Command, own_session: bool) -> Result<Child, SessionError> {
    // SAFETY: the closure runs in the child between fork and exec, where only async-signal-safe
    // calls are allowed: umask() and setsid() are, and nothing in it allocates or takes a lock.
    unsafe {
        command.pre_exec(move || {
            libc::umask(0o077);
            if own_session && libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().map_err(|error| SessionError::HelperStart(format!("{:?}", error.kind())))
}

/// The native sign-in in the terminal this process runs in: the helper asks for the Apple ID,
/// the password (hidden) and the verification code there. True when it produced a fresh sign-in,
/// which requests then use; otherwise an earlier sign-in on this machine is kept.
pub fn android_signin_terminal(web: &Web) -> Result<bool, SessionError> {
    let command = helper_command(web.config(), &["login", "--stop-service"])?;
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(SessionError::NoTerminal);
    }
    let done = start(command, false)?.wait();
    if !done.is_ok_and(|status| status.success()) {
        return Ok(false);
    }
    web.reset_token_sources(); // the new sign-in is the one to use
    write_android_status(web.config(), None); // not checked yet
    Ok(true)
}

/// Start the native sign-in with these credentials. An error when it cannot run here.
///
/// The helper is `wrapperctl.py login --pipe --stop-service`. It reads one JSON object per line:
/// `{"username": ..., "password": ...}` first, `{"code": "123456"}` after the "code_required"
/// phase, `{"cancel": true}` at any time (closing its input cancels too). It writes one per
/// line: `{"event": "phase", "phase": ...}`, `{"event": "notice", "error": ...}` for an input
/// line it did not take, and `{"event": "result", "ok": ..., "kind": ..., "error": ...,
/// "steps": [...], "log": ...}` before it exits.
pub fn android_signin(
    web: Arc<Web>,
    username: &str,
    password: &str,
    on_event: Option<OnEvent>,
) -> Result<AndroidSignIn, SessionError> {
    start_signin(web, username, password, on_event, CANCEL_GRACE)
}

fn start_signin(
    web: Arc<Web>,
    username: &str,
    password: &str,
    on_event: Option<OnEvent>,
    grace: Duration,
) -> Result<AndroidSignIn, SessionError> {
    let mut command = helper_command(web.config(), &["login", "--pipe", "--stop-service"])?;
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let signin = AndroidSignIn::follow(start(command, true)?, web, on_event, grace)?;
    // when the helper has ended already, its result says why
    signin.shared.send(json_line(&[("username", username), ("password", password)]));
    Ok(signin)
}

#[cfg(test)]
mod tests {
    //! No network, no Apple library and no real account data: requests go to a stand-in network,
    //! the sign-in helper is a stand-in script in a scratch folder (tests/fixtures), and tokens,
    //! names and passwords are made up.
    use super::*;
    use crate::web::testing::{ANDROID_TOKEN, Answer, BROWSER_TOKEN, FakeTransport, Seen, sign_in};
    use sha2::{Digest, Sha256};
    use std::collections::{BTreeMap, HashMap};
    use std::path::PathBuf;

    const USERNAME: &str = "someone@example.com";
    /// Takes the place of tools/lossless/wrapperctl.py; the password picks what it does.
    const STAND_IN: &str = include_str!("../tests/fixtures/wrapperctl_stand_in.py");
    const LIBRARY_PAGE: &str = r#"{"data": [{"id": "x"}], "next": "/more"}"#;
    /// Longer than any stand-in sign-in takes.
    const LONG: Option<Duration> = Some(Duration::from_secs(30));

    /// A scratch am-ui folder and a network that answers by the cookie a request carries.
    struct Scratch {
        _dir: Option<tempfile::TempDir>,
        config: Arc<Config>,
        web: Arc<Web>,
        seen: Arc<Mutex<Vec<Seen>>>,
        /// token -> HTTP status ("" for requests without one)
        verdicts: Arc<Mutex<HashMap<String, u16>>>,
        /// What an accepted request is answered with.
        page: Arc<Mutex<String>>,
    }

    impl Scratch {
        fn new(env: &[(&str, &str)]) -> Scratch {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("am-ui");
            fs::create_dir_all(root.join("workspace")).unwrap();
            Scratch { _dir: Some(dir), ..Scratch::over(root, env) }
        }

        /// Configuration and network over a scratch folder that exists already.
        fn over(root: PathBuf, env: &[(&str, &str)]) -> Scratch {
            let env = env.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect();
            let config = Arc::new(Config::new(root.clone(), &root.with_file_name("home"), &env));
            config.ensure_dirs().unwrap();
            let verdicts = Arc::new(Mutex::new(HashMap::from([
                (ANDROID_TOKEN.to_string(), 200),
                (BROWSER_TOKEN.to_string(), 200),
            ])));
            let page = Arc::new(Mutex::new(LIBRARY_PAGE.to_string()));
            let (known, answer) = (verdicts.clone(), page.clone());
            let (transport, seen) = FakeTransport::new(move |request| {
                let status = known.lock().unwrap().get(&request.token().unwrap_or_default()).copied().unwrap_or(500);
                Ok(if status == 200 { Answer::ok(answer.lock().unwrap().as_bytes()) } else { Answer::status(status) })
            });
            let web = Arc::new(Web::with_transport(config.clone(), Box::new(transport)));
            web.preset_developer_token("developer", util::unix_now() + 90.0 * 86400.0);
            Scratch { _dir: None, config, web, seen, verdicts, page }
        }

        /// A scratch folder holding a built and staged lossless route whose sign-in helper is the
        /// stand-in.
        fn with_route(env: &[(&str, &str)]) -> Scratch {
            let scratch = Scratch::new(env);
            let config = &scratch.config;
            let runtime = config.lossless_data_dir.parent().unwrap();
            for (path, text) in [
                (config.lossless_wrapperctl.clone(), STAND_IN),
                (config.lossless_build_dir.join("bin/wrapper-lite-rootless"), "stand-in\n"),
                (runtime.join("system/bin/lite"), "stand-in\n"),
            ] {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, text).unwrap();
            }
            util::create_private_dirs(&config.lossless_data_dir).unwrap();
            assert!(android_signin_available(config));
            scratch
        }

        fn sign_in(&self, sources: &[Source]) {
            for source in sources {
                let token = if *source == Source::Android { ANDROID_TOKEN } else { BROWSER_TOKEN };
                sign_in(&self.config, *source, token);
            }
        }

        /// What Apple answers to requests that carry this token.
        fn verdict(&self, token: &str, status: u16) {
            self.verdicts.lock().unwrap().insert(token.to_string(), status);
        }

        /// The tokens of the requests made, in their order.
        fn tokens(&self) -> Vec<String> {
            self.seen.lock().unwrap().iter().filter_map(Seen::token).collect()
        }

        /// Account files of a sign-in made before the test.
        fn earlier_sign_in(&self) {
            for name in ["DEV_TOKEN", "MUSIC_TOKEN", "STOREFRONT_ID"] {
                let path = self.config.lossless_data_dir.join(name);
                fs::write(&path, format!("old-{name}\n")).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            }
        }

        /// The (made-up) account files and what they hold.
        fn account(&self) -> BTreeMap<String, String> {
            fs::read_dir(&self.config.lossless_data_dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.is_file())
                .map(|path| {
                    let name = path.file_name().unwrap().to_string_lossy().into_owned();
                    (name, fs::read_to_string(&path).unwrap().trim().to_string())
                })
                .collect()
        }

        /// Where the stand-in says how it was started; missing when it never was.
        fn report_file(&self) -> PathBuf {
            self.config.var.join("wrapper/stub-report.json")
        }

        fn report(&self) -> Value {
            util::read_json(&self.report_file()).expect("the stand-in's report")
        }

        /// A verdict of an earlier check of the native sign-in.
        fn earlier_verdict(&self) -> &Path {
            let file = &self.config.android_session_status_file;
            fs::write(file, r#"{"state": "reconnect_required"}"#).unwrap();
            file
        }
    }

    fn account_files(prefix: &str) -> BTreeMap<String, String> {
        ["DEV_TOKEN", "MUSIC_TOKEN", "STOREFRONT_ID"]
            .iter()
            .map(|name| (name.to_string(), format!("{prefix}-{name}")))
            .collect()
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn keys(path: &Path) -> Vec<String> {
        util::read_json_object(path).unwrap().keys().cloned().collect()
    }

    fn wait_for(condition: impl Fn() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A listener that keeps what it receives.
    fn recorder() -> (Arc<Mutex<Vec<SignInEvent>>>, OnEvent) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        (events, Box::new(move |event| sink.lock().unwrap().push(event)))
    }

    fn phases(events: &[SignInEvent]) -> Vec<Phase> {
        events
            .iter()
            .filter_map(|event| match event {
                SignInEvent::Phase(phase) => Some(*phase),
                _ => None,
            })
            .collect()
    }

    /// Ends the stand-in when its test is over, a failed one too.
    struct StopAtEnd<'a>(&'a AndroidSignIn);

    impl Drop for StopAtEnd<'_> {
        fn drop(&mut self) {
            self.0.shared.kill();
        }
    }

    fn stopped() -> SignInResult {
        SignInResult {
            ok: false,
            kind: Some("failed".into()),
            error: Some("The sign-in helper stopped unexpectedly".into()),
            steps: vec![],
            log: None,
        }
    }

    #[test]
    fn names_are_those_of_the_python_module() {
        assert_eq!(State::ALL.iter().map(State::as_str).collect::<Vec<_>>(), STATES);
        assert_eq!(Phase::ALL.iter().map(Phase::as_str).collect::<Vec<_>>(), SIGNIN_PHASES);
        assert_eq!(Route::ALL.iter().map(Route::to_string).collect::<Vec<_>>(), ["android", "none"]);
        assert_eq!(State::parse("reconnect_required"), Some(State::ReconnectRequired));
        assert_eq!((Phase::parse("code_required"), Phase::parse("dancing")), (Some(Phase::CodeRequired), None));
        assert_eq!(
            SessionError::NotBuilt.to_string(),
            "The native sign-in needs the lossless route, which is not built on this machine \
             (./setup.sh builds it on x86_64)"
        );
        assert_eq!(
            SessionError::NoTerminal.to_string(),
            "The native sign-in asks for the password and the verification code in a terminal: \
             run this in one, or use the Sign In button of the app"
        );
    }

    #[test]
    fn times_are_written_as_python_writes_them() {
        // datetime.fromtimestamp(seconds, timezone.utc).isoformat()
        for (seconds, micros, text) in [
            (0, 0, "1970-01-01T00:00:00+00:00"),
            (1_767_225_600, 0, "2026-01-01T00:00:00+00:00"),
            (1_709_210_096, 789_012, "2024-02-29T12:34:56.789012+00:00"),
            (951_782_400, 0, "2000-02-29T00:00:00+00:00"),
            (4_102_444_799, 1, "2099-12-31T23:59:59.000001+00:00"),
            (1_700_000_000, 500_000, "2023-11-14T22:13:20.500000+00:00"),
            (1_709_251_199, 999_999, "2024-02-29T23:59:59.999999+00:00"),
        ] {
            assert_eq!(utc_isoformat(Duration::new(seconds, micros * 1000)), text);
        }
        let written = regex::Regex::new(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d{6})?\+00:00$").unwrap();
        assert!(written.is_match(&now()));
    }

    #[test]
    fn answers_as_the_dictionaries_of_the_python_module() {
        let result = SignInResult {
            ok: false,
            kind: Some("code".into()),
            error: Some("Refused".into()),
            steps: vec!["signing_in".into(), "code_prompt".into()],
            log: Some("/scratch/signin-last.log".into()),
        };
        let as_json = concat!(
            r#"{"ok":false,"kind":"code","error":"Refused","#,
            r#""steps":["signing_in","code_prompt"],"log":"/scratch/signin-last.log"}"#
        );
        assert_eq!(result.to_json().to_string(), as_json);
        assert_eq!(
            SignInEvent::Result(result).to_json().to_string(),
            format!(r#"{{"event":"result",{}"#, &as_json[1..])
        );
        assert_eq!(
            SignInEvent::Phase(Phase::CodeRequired).to_json(),
            json!({"event": "phase", "phase": "code_required"})
        );
        assert_eq!(
            SignInEvent::Notice { error: "Six digits".into() }.to_json(),
            json!({"event": "notice", "error": "Six digits"})
        );
        assert_eq!(
            stopped().to_json(),
            json!({"ok": false, "kind": "failed", "error": HELPER_STOPPED, "steps": [], "log": null})
        );
    }

    #[test]
    fn status_files_give_and_take_the_safe_keys_only() {
        let scratch = Scratch::new(&[]);
        let file = &scratch.config.android_session_status_file;
        let everything = json!({
            "unexpected": "must-not-appear", "state": "connected", "checked_at": "then", "cookie_expires_at": null,
            "library": {"songs": {}}, "captured_at": "earlier", "source": "somewhere", "http_status": 200,
            "token": "must-not-appear",
        });
        fs::write(file, everything.to_string()).unwrap();
        let read = read_status_file(&scratch.config, Source::Android);
        assert_eq!(read.keys().map(String::as_str).collect::<Vec<_>>(), SAFE_KEYS);
        assert_eq!((&read["http_status"], &read["library"]), (&json!(200), &json!({"songs": {}})));
        // and whatever a record holds, the file gets no more than that
        write_status_file(file, everything.as_object().unwrap()).unwrap();
        let written = fs::read_to_string(file).unwrap();
        assert!(!written.contains("must-not-appear"), "{written}");
        assert_eq!(
            keys(file),
            ["state", "checked_at", "cookie_expires_at", "library", "captured_at", "source", "http_status"]
        );
        assert_eq!(mode(file), 0o600);
    }

    #[test]
    fn a_sign_in_can_be_handed_to_other_threads() {
        fn shared_between_threads<T: Send + Sync>() {}
        shared_between_threads::<AndroidSignIn>();
        shared_between_threads::<SignInEvent>();
        shared_between_threads::<Status>();
    }

    mod session_status {
        //! Status file handling (SessionTest of tests/test_api_parsing.py, its part about
        //! `status`).
        use super::*;

        #[test]
        fn status_without_token() {
            let scratch = Scratch::new(&[]);
            let state = status(&scratch.web);
            assert_eq!(
                (state.state, state.has_token, &state.checked_at, &state.library),
                (State::NoSession, false, &None, &None)
            );
            assert_eq!(
                state.to_json().to_string(),
                concat!(
                    r#"{"state":"no_session","checked_at":null,"cookie_expires_at":null,"library":null,"#,
                    r#""captured_at":null,"http_status":null,"has_token":false,"signin":null,"signin_route":"none"}"#
                )
            );
        }

        #[test]
        fn status_with_token_but_no_file() {
            let scratch = Scratch::new(&[]);
            scratch.sign_in(&[Source::Browser]);
            assert_eq!(status(&scratch.web).state, State::Unknown);
        }

        #[test]
        fn status_copies_only_safe_keys() {
            let scratch = Scratch::new(&[]);
            scratch.sign_in(&[Source::Browser]);
            let saved = json!({
                "state": "reconnect_required", "checked_at": "2026-01-01T00:00:00+00:00", "http_status": 403,
                "library": {"songs": {"accessible": true}}, "unexpected": "must-not-appear",
            });
            fs::write(&scratch.config.session_status_file, saved.to_string()).unwrap();
            let state = status(&scratch.web);
            assert_eq!((state.state, state.http_status, state.has_token), (State::ReconnectRequired, Some(403), true));
            assert_eq!(state.library, json!({"songs": {"accessible": true}}).as_object().cloned());
            assert!(!state.to_json().to_string().contains("must-not-appear"));
            assert!(!format!("{state:?}").contains("must-not-appear"));
            assert_eq!(state.to_json()["signin"], "browser");
        }

        #[test]
        fn a_status_file_is_read_only_when_it_is_a_plain_file_holding_an_object() {
            let scratch = Scratch::new(&[]);
            scratch.sign_in(&[Source::Android]);
            let file = &scratch.config.android_session_status_file;
            let elsewhere = scratch.config.tmp_dir.join("elsewhere.json");
            fs::write(&elsewhere, r#"{"state": "connected"}"#).unwrap();
            std::os::unix::fs::symlink(&elsewhere, file).unwrap();
            assert_eq!(status(&scratch.web).state, State::Unknown); // a link is not followed
            fs::remove_file(file).unwrap();
            fs::rename(&elsewhere, file).unwrap();
            assert_eq!(status(&scratch.web).state, State::Connected);
            for (text, expected) in [
                (r#"["connected"]"#, (State::Unknown, None)),
                ("not json", (State::Unknown, None)),
                (r#"{"state": "no_session", "checked_at": 5, "http_status": "401"}"#, (State::Unknown, None)),
                (r#"{"state": "connected", "checked_at": "then", "library": [1]}"#, (State::Connected, Some("then"))),
            ] {
                fs::write(file, text).unwrap();
                let state = status(&scratch.web);
                assert_eq!((state.state, state.checked_at.as_deref()), expected, "{text}");
                assert_eq!((&state.library, state.http_status), (&None, None), "{text}");
            }
        }
    }

    mod token_source {
        //! `status` and `check` on top of the two sign-ins of `web` (the end of TokenSourceTest of
        //! tests/test_signin.py), and the check of a saved browser session, which the Python
        //! module left to the workspace's tools/music_session.py.
        use super::*;

        #[test]
        fn status_names_the_sign_in_in_use() {
            let scratch = Scratch::new(&[]);
            scratch.sign_in(&[Source::Android, Source::Browser]);
            let saved = r#"{"state": "connected", "checked_at": "2026-01-01T00:00:00+00:00"}"#;
            fs::write(&scratch.config.session_status_file, saved).unwrap();
            let state = status(&scratch.web);
            assert_eq!((state.signin, state.state, state.has_token), (Some(Source::Android), State::Unknown, true));
            scratch.web.demote_source(Source::Android);
            let state = status(&scratch.web);
            assert_eq!((state.signin, state.state), (Some(Source::Browser), State::Connected));
        }

        #[test]
        fn check_of_the_native_sign_in() {
            let scratch = Scratch::new(&[]);
            scratch.sign_in(&[Source::Android]);
            let state = check(&scratch.web).unwrap();
            assert_eq!((state.state, state.signin), (State::Connected, Some(Source::Android)));
            assert_eq!(
                state.library.as_ref().unwrap()["songs"],
                json!({"accessible": true, "sample_count": 1, "has_more": true})
            );
            assert_eq!(scratch.tokens().len(), 2);
            let file = &scratch.config.android_session_status_file;
            let saved = fs::read_to_string(file).unwrap();
            assert!(!saved.contains(ANDROID_TOKEN));
            assert_eq!(mode(file), 0o600);
        }

        #[test]
        fn check_rejected() {
            let scratch = Scratch::new(&[]);
            scratch.sign_in(&[Source::Android]);
            scratch.verdict(ANDROID_TOKEN, 401);
            let state = check(&scratch.web).unwrap();
            assert_eq!(
                (state.state, state.http_status, state.signin),
                (State::ReconnectRequired, Some(401), Some(Source::Android))
            );
        }

        #[test]
        fn check_moves_to_the_other_sign_in_when_apple_accepts_it() {
            let scratch = Scratch::new(&[]);
            scratch.sign_in(&[Source::Android, Source::Browser]);
            scratch.verdict(ANDROID_TOKEN, 401);
            let browser_file = scratch.config.session_status_file.clone();
            let browser_check = |_: &Web| {
                fs::write(&browser_file, r#"{"state": "connected"}"#).unwrap();
                Ok(State::Connected)
            };
            let state = check_with(&scratch.web, browser_check).unwrap();
            assert_eq!((state.state, state.signin), (State::Connected, Some(Source::Browser)));
            assert_eq!(scratch.tokens(), [ANDROID_TOKEN]); // the check pins each sign-in: no silent second attempt
            let saved = util::read_json_object(&scratch.config.android_session_status_file).unwrap();
            assert_eq!(saved["state"], "reconnect_required");
        }

        // --- beyond the Python tests

        #[test]
        fn check_stops_at_an_outcome_that_is_neither_yes_nor_no() {
            let scratch = Scratch::new(&[("AMUI_SIGNIN", "browser")]);
            scratch.sign_in(&[Source::Android, Source::Browser]);
            assert_eq!(scratch.web.token_sources(), [Source::Browser, Source::Android]);
            let state = check_with(&scratch.web, |_| Ok(State::Unknown)).unwrap();
            assert_eq!((state.state, state.signin), (State::Unknown, Some(Source::Browser)));
            assert!(scratch.tokens().is_empty()); // only a rejection makes the check ask the other sign-in
            let error = check_with(&scratch.web, |_| Err(SessionError::Check("no answer".into()))).unwrap_err();
            assert_eq!(error.to_string(), "no answer");
            assert!(scratch.tokens().is_empty());
        }

        #[test]
        fn check_asks_the_other_sign_in_too_and_stays_with_the_first_when_both_are_rejected() {
            let scratch = Scratch::new(&[]);
            scratch.sign_in(&[Source::Android, Source::Browser]);
            scratch.verdict(ANDROID_TOKEN, 401);
            let state = check(&scratch.web).unwrap();
            assert_eq!((state.state, state.signin), (State::Connected, Some(Source::Browser)));
            assert_eq!(scratch.tokens(), [ANDROID_TOKEN, BROWSER_TOKEN, BROWSER_TOKEN]);
            assert_eq!(scratch.web.token_sources(), [Source::Browser, Source::Android]);

            scratch.web.reset_token_sources();
            scratch.verdict(BROWSER_TOKEN, 403);
            let state = check(&scratch.web).unwrap();
            assert_eq!(
                (state.state, state.http_status, state.signin),
                (State::ReconnectRequired, Some(401), Some(Source::Android))
            );
            assert_eq!(scratch.web.token_sources(), [Source::Android, Source::Browser]); // nobody was demoted
            assert_eq!(scratch.tokens()[3..], [ANDROID_TOKEN, BROWSER_TOKEN]);
        }

        #[test]
        fn check_of_the_saved_browser_session() {
            let scratch = Scratch::new(&[]);
            scratch.sign_in(&[Source::Browser]);
            let file = &scratch.config.session_status_file;
            let folder = file.parent().unwrap();
            // what the sign-in recorded about itself is kept, and nothing else of the old file
            let old = json!({
                "state": "reconnect_required", "checked_at": "2026-01-01T00:00:00+00:00", "http_status": 401,
                "source": "dedicated-chromium-profile", "captured_at": "2025-12-01T00:00:00+00:00",
                "cookie_expires_at": null, "unexpected": "must-not-appear",
            });
            fs::write(file, old.to_string()).unwrap();
            fs::set_permissions(folder, fs::Permissions::from_mode(0o755)).unwrap();
            *scratch.page.lock().unwrap() = r#"{"data": []}"#.to_string();

            let state = check(&scratch.web).unwrap();
            assert_eq!((state.state, state.signin, state.http_status), (State::Connected, Some(Source::Browser), None));
            assert_eq!(state.captured_at.as_deref(), Some("2025-12-01T00:00:00+00:00"));
            assert_ne!(state.checked_at.as_deref(), Some("2026-01-01T00:00:00+00:00"));
            let empty = json!({"accessible": true, "sample_count": 0, "has_more": false});
            assert_eq!(state.library, json!({"playlists": empty, "songs": empty}).as_object().cloned());
            // the same two requests, with this sign-in's cookie
            let requests: Vec<String> = scratch.seen.lock().unwrap().iter().map(|seen| seen.url.clone()).collect();
            assert_eq!(
                requests,
                [
                    "https://amp-api.music.apple.com/v1/me/library/playlists?limit=1",
                    "https://amp-api.music.apple.com/v1/me/library/songs?limit=1"
                ]
            );
            assert_eq!(scratch.tokens(), [BROWSER_TOKEN, BROWSER_TOKEN]);
            assert_eq!(keys(file), ["state", "checked_at", "library", "captured_at", "cookie_expires_at", "source"]);
            let saved = fs::read_to_string(file).unwrap();
            assert!(!saved.contains("must-not-appear") && !saved.contains(BROWSER_TOKEN));
            assert_eq!((mode(file), mode(folder)), (0o600, 0o700)); // private, next to the token
        }

        #[test]
        fn check_of_the_saved_browser_session_rejected() {
            let scratch = Scratch::new(&[]);
            scratch.sign_in(&[Source::Browser]);
            scratch.verdict(BROWSER_TOKEN, 403);
            let file = &scratch.config.session_status_file;
            fs::write(file, r#"{"state": "connected", "captured_at": "2025-12-01T00:00:00+00:00"}"#).unwrap();
            let state = check(&scratch.web).unwrap();
            assert_eq!(
                (state.state, state.http_status, state.signin),
                (State::ReconnectRequired, Some(403), Some(Source::Browser))
            );
            assert_eq!(scratch.tokens(), [BROWSER_TOKEN]); // Apple's first answer is enough
            assert_eq!(keys(file), ["state", "checked_at", "http_status"]);
            assert_eq!(mode(file), 0o600);
        }

        #[test]
        fn a_check_that_cannot_be_made_is_an_error() {
            let scratch = Scratch::new(&[]);
            scratch.sign_in(&[Source::Android, Source::Browser]);
            scratch.verdict(ANDROID_TOKEN, 500);
            let error = check(&scratch.web).unwrap_err();
            assert_eq!(error, SessionError::Check("amp-api.music.apple.com returned HTTP 500".into()));
            assert_eq!(scratch.tokens(), [ANDROID_TOKEN]); // not a rejection: the other sign-in is not asked
            assert!(!scratch.config.android_session_status_file.exists());
            assert_eq!(status(&scratch.web).state, State::Unknown);

            scratch.verdict(ANDROID_TOKEN, 401);
            scratch.verdict(BROWSER_TOKEN, 429);
            let error = check(&scratch.web).unwrap_err();
            assert_eq!(error.to_string(), "amp-api.music.apple.com rate-limited this session");
            assert!(!scratch.config.session_status_file.exists());
            for secret in [ANDROID_TOKEN, BROWSER_TOKEN] {
                assert!(!format!("{error:?}").contains(secret));
            }

            // an answer that is no library page
            scratch.web.demote_source(Source::Android);
            scratch.verdict(BROWSER_TOKEN, 200);
            for page in [r#"{"data": {"id": "x"}}"#, r#"{"next": "/more"}"#] {
                *scratch.page.lock().unwrap() = page.to_string();
                let error = check(&scratch.web).unwrap_err();
                assert_eq!(error.to_string(), "Apple did not return the expected library response");
            }
            assert!(!scratch.config.session_status_file.exists());
            *scratch.page.lock().unwrap() = "<html>".to_string();
            let error = check(&scratch.web).unwrap_err();
            assert_eq!(error.to_string(), "Apple returned a response that is not JSON");
        }

        #[test]
        fn a_refusal_of_the_public_token_is_not_a_rejection_of_the_sign_in() {
            let scratch = Scratch::new(&[]);
            scratch.sign_in(&[Source::Android, Source::Browser]);
            // no current developer token: `web` asks music.apple.com for one, without credentials
            scratch.web.preset_developer_token("expired", 0.0);
            scratch.verdict("", 403);
            let error = check(&scratch.web).unwrap_err();
            assert!(error.to_string().starts_with("music.apple.com "), "{error}");
            let seen = scratch.seen.lock().unwrap();
            assert!(seen.len() == 1 && seen[0].url.starts_with("https://music.apple.com/"), "{seen:?}");
            assert!(seen[0].token().is_none()); // no account request was made
            let config = &scratch.config;
            assert!(!config.android_session_status_file.exists() && !config.session_status_file.exists());
            assert_eq!(status(&scratch.web).state, State::Unknown); // no verdict about either sign-in
        }

        #[test]
        fn a_sign_in_file_that_cannot_be_used() {
            // the native one: it takes a new sign-in, as after a rejection, without a status of Apple's
            let scratch = Scratch::new(&[]);
            scratch.sign_in(&[Source::Android]);
            fs::write(scratch.config.lossless_data_dir.join("MUSIC_TOKEN"), "not a token\n").unwrap();
            let state = check(&scratch.web).unwrap();
            assert_eq!((state.state, state.http_status), (State::ReconnectRequired, None));
            assert_eq!(keys(&scratch.config.android_session_status_file), ["state", "checked_at"]);
            // the browser session: the problem is named
            scratch.sign_in(&[Source::Browser]);
            scratch.web.demote_source(Source::Android);
            fs::set_permissions(&scratch.config.token_file, fs::Permissions::from_mode(0o644)).unwrap();
            let error = check(&scratch.web).unwrap_err();
            assert_eq!(error, SessionError::Check("Saved token must be a private file with mode 0600".into()));
            assert!(!scratch.config.session_status_file.exists());
            assert!(scratch.tokens().is_empty()); // nothing was sent anywhere
        }

        #[test]
        fn a_browser_status_that_cannot_be_kept_is_an_error() {
            let scratch = Scratch::new(&[]);
            scratch.sign_in(&[Source::Browser]);
            fs::create_dir(&scratch.config.session_status_file).unwrap(); // a folder where the file belongs
            let error = check(&scratch.web).unwrap_err();
            assert!(error.to_string().starts_with("Session check failed ("), "{error}");
            assert_eq!(scratch.tokens().len(), 2);
        }

        #[test]
        fn status_files_are_written_as_the_python_module_writes_them() {
            let scratch = Scratch::new(&[]);
            scratch.sign_in(&[Source::Android, Source::Browser]);
            let (android, browser) = (&scratch.config.android_session_status_file, &scratch.config.session_status_file);
            fs::write(browser, r#"{"source": "café 🔑", "captured_at": "2025-12-01T00:00:00+00:00"}"#).unwrap();
            check(&scratch.web).unwrap();
            scratch.web.demote_source(Source::Android);
            check(&scratch.web).unwrap();

            let page = "{\n      \"accessible\": true,\n      \"sample_count\": 1,\n      \"has_more\": true\n    }";
            let record = |checked_at: &Value, more: &str| {
                format!(
                    "{{\n  \"state\": \"connected\",\n  \"checked_at\": {checked_at},\n  \"library\": {{\n    \
                     \"playlists\": {page},\n    \"songs\": {page}\n  }}{more}\n}}\n"
                )
            };
            let checked_at = &util::read_json_object(android).unwrap()["checked_at"];
            assert_eq!(fs::read_to_string(android).unwrap(), record(checked_at, ""));
            let checked_at = &util::read_json_object(browser).unwrap()["checked_at"];
            let kept =
                ",\n  \"captured_at\": \"2025-12-01T00:00:00+00:00\",\n  \"source\": \"caf\\u00e9 \\ud83d\\udd11\"";
            assert_eq!(fs::read_to_string(browser).unwrap(), record(checked_at, kept));
            let written = regex::Regex::new(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d{6})?\+00:00$").unwrap();
            assert!(written.is_match(checked_at.as_str().unwrap()), "{checked_at}");

            // Python reads both files and writes them back byte for byte
            let round_trip = "import json, sys\n\
                              for path in sys.argv[1:]:\n    \
                              text = open(path, encoding='utf-8').read()\n    \
                              assert json.dumps(json.loads(text), indent=2) + '\\n' == text, path\n";
            let python =
                Command::new("python3").args(["-I", "-c", round_trip]).args([android, browser]).output().unwrap();
            assert!(python.status.success(), "{}", String::from_utf8_lossy(&python.stderr));
        }
    }

    mod android_sign_in {
        //! `AndroidSignIn` over the stand-in (AndroidSignInTest of tests/test_signin.py), and what
        //! the helper is given and how it is ended.
        use super::*;
        use std::os::unix::ffi::OsStrExt;

        #[test]
        fn sign_in_with_code() {
            let scratch = Scratch::with_route(&[]);
            let verdict = scratch.earlier_verdict();
            scratch.sign_in(&[Source::Browser]); // so that the order of the sign-ins shows
            scratch.web.demote_source(Source::Android);
            let (events, listener) = recorder();
            let signin = android_signin(scratch.web.clone(), USERNAME, "pw-2fa", Some(listener)).unwrap();
            let _stop = StopAtEnd(&signin);
            wait_for(|| signin.phase() == Phase::CodeRequired, "the code request");
            assert!(signin.running());
            assert_eq!(signin.result(), None);
            assert!(signin.submit_code(" 123456 "));
            let done = SignInResult { ok: true, kind: None, error: None, steps: vec![], log: None };
            assert_eq!(signin.wait(LONG), Some(done.clone()));
            let events = events.lock().unwrap();
            assert_eq!(events.last(), Some(&SignInEvent::Result(done)));
            assert_eq!(
                phases(&events),
                [Phase::Preparing, Phase::SigningIn, Phase::CodeRequired, Phase::Verifying, Phase::Finishing]
            );
            assert!(!verdict.exists()); // the old verdict is forgotten
            assert_eq!(scratch.web.token_sources()[0], Source::Android); // and the new sign-in is used first
            assert!(!signin.submit_code("123456")); // nothing listens any more
            assert_eq!(scratch.report()["codes"], 1);
            assert!(scratch.seen.lock().unwrap().is_empty()); // a sign-in asks Apple nothing through `web`
        }

        #[test]
        fn failure_and_cancel() {
            let scratch = Scratch::with_route(&[]);
            let signin = android_signin(scratch.web.clone(), USERNAME, "pw-wrong", None).unwrap();
            let result = signin.wait(LONG).unwrap();
            assert_eq!((result.ok, result.kind.as_deref()), (false, Some("credentials")));
            assert_eq!(result.steps, ["signing_in", "password_request"]);
            let log = scratch.config.var.join("wrapper/logs/signin-last.log");
            assert_eq!(result.log.as_deref(), log.to_str());
            let signin = android_signin(scratch.web.clone(), USERNAME, "pw-hang", None).unwrap();
            let _stop = StopAtEnd(&signin);
            wait_for(|| signin.phase() == Phase::SigningIn, "the sign-in to start");
            let started = Instant::now();
            signin.cancel();
            assert!(started.elapsed() < Duration::from_secs(1));
            let result = signin.wait(LONG).unwrap();
            assert_eq!((result.ok, result.kind.as_deref()), (false, Some("cancelled")));
            // by the cancel itself: nothing had to stop the helper
            assert!(started.elapsed() < Duration::from_secs(10));
            signin.cancel(); // harmless afterwards
        }

        #[test]
        fn listener_errors_do_not_stop_the_sign_in() {
            let scratch = Scratch::with_route(&[]);
            let listener: OnEvent = Box::new(|_| std::panic::resume_unwind(Box::new("listener problem")));
            let signin = android_signin(scratch.web.clone(), USERNAME, "pw-plain", Some(listener)).unwrap();
            assert!(signin.wait(LONG).unwrap().ok);
        }

        #[test]
        fn unavailable_without_the_lossless_route() {
            let scratch = Scratch::with_route(&[]);
            fs::remove_file(scratch.config.lossless_data_dir.parent().unwrap().join("system/bin/lite")).unwrap();
            assert!(!android_signin_available(&scratch.config));
            let error = android_signin(scratch.web.clone(), USERNAME, "pw-plain", None).unwrap_err();
            assert_eq!(error, SessionError::NotBuilt);
            assert_eq!(signin_route(&scratch.config), Route::None); // no other sign-in to offer here
            assert_eq!(android_signin_terminal(&scratch.web), Err(SessionError::NotBuilt));
            assert!(!scratch.report_file().exists()); // nothing was started
        }

        #[test]
        fn route() {
            let scratch = Scratch::with_route(&[]);
            assert_eq!(signin_route(&scratch.config), Route::Android);
            // asking for the browser changes the order of saved sign-ins, not what Connect does:
            // this engine has no browser sign-in to offer
            let scratch = Scratch::with_route(&[("AMUI_SIGNIN", "browser")]);
            assert!(scratch.config.prefer_browser_signin);
            assert_eq!(signin_route(&scratch.config), Route::Android);
            assert_eq!(status(&scratch.web).signin_route, Route::Android);
        }

        // --- beyond the Python tests

        #[test]
        fn the_helper_is_started_as_the_python_module_starts_it() {
            let scratch = Scratch::with_route(&[]);
            let script = &scratch.config.lossless_wrapperctl;
            let command = helper_command(&scratch.config, &["login", "--pipe", "--stop-service"]).unwrap();
            assert_eq!(command.get_program(), "python3");
            let arguments: Vec<_> = command.get_args().collect();
            assert_eq!(
                arguments,
                ["-I".as_ref(), script.as_os_str(), "login".as_ref(), "--pipe".as_ref(), "--stop-service".as_ref()]
            );
            assert_eq!(command.get_envs().count(), 0); // nothing is added to its environment

            let signin = android_signin(scratch.web.clone(), USERNAME, "pw-hang", None).unwrap();
            let _stop = StopAtEnd(&signin);
            wait_for(|| signin.phase() == Phase::SigningIn, "the sign-in to start");
            let report = scratch.report();
            assert_eq!(report["argv"], json!(["login", "--pipe", "--stop-service"]));
            assert_eq!(report["isolated"], true); // python3 -I
            assert_eq!(report["umask"], 0o077); // what it creates is private
            assert_eq!(report["own_session"], true); // no terminal of this process
            assert_eq!(report["streams"], json!(["pipe", "pipe", "null"]));
            signin.cancel();
            assert_eq!(signin.wait(LONG).unwrap().kind.as_deref(), Some("cancelled"));
        }

        #[test]
        fn what_is_typed_goes_through_the_pipe_only() {
            let scratch = Scratch::with_route(&[]);
            let (username, password) = ("typed.name@example.com", "pw-2fa typed-password");
            let signin = android_signin(scratch.web.clone(), username, password, None).unwrap();
            let _stop = StopAtEnd(&signin);
            wait_for(|| signin.phase() == Phase::CodeRequired, "the code request");
            // the command line and the environment of the running helper, as the system shows them
            let process = PathBuf::from(format!("/proc/{}", signin.shared.pid));
            let command_line = fs::read(process.join("cmdline")).unwrap();
            let arguments: Vec<&[u8]> = command_line.split(|byte| *byte == 0).filter(|part| !part.is_empty()).collect();
            let script = scratch.config.lossless_wrapperctl.as_os_str().as_bytes();
            assert_eq!(arguments[1..], [b"-I".as_slice(), script, b"login", b"--pipe", b"--stop-service"]);
            let environment = fs::read(process.join("environ")).unwrap();
            for shown in [&command_line, &environment] {
                let shown = String::from_utf8_lossy(shown);
                assert!(!shown.contains(username) && !shown.contains(password));
            }
            // and as the helper saw them itself
            let report = scratch.report();
            assert_eq!(report["typed_in_arguments_or_environment"], false);
            assert_eq!(report["argv"], json!(["login", "--pipe", "--stop-service"]));
            // no file of this application holds any of it either
            signin.cancel();
            signin.wait(LONG).unwrap();
            let mut folders = vec![scratch.config.root.clone()];
            while let Some(folder) = folders.pop() {
                for entry in fs::read_dir(folder).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        folders.push(path);
                    } else if path != scratch.config.lossless_wrapperctl {
                        let text = String::from_utf8_lossy(&fs::read(&path).unwrap()).into_owned();
                        assert!(!text.contains(username) && !text.contains(password), "{}", path.display());
                    }
                }
            }
        }

        #[test]
        fn nothing_typed_shows_up_in_events_results_or_debug_output() {
            let scratch = Scratch::with_route(&[]);
            let username = "typed.name@example.com";
            let debug = |signin: &AndroidSignIn| format!("{signin:?}").replace(&signin.shared.pid.to_string(), "pid");
            for (scenario, code) in
                [("pw-2fa", Some("123456")), ("pw-2fa", Some("654321")), ("pw-wrong", None), ("pw-crash", None)]
            {
                let password = format!("{scenario} typed-password");
                let (events, listener) = recorder();
                let signin = android_signin(scratch.web.clone(), username, &password, Some(listener)).unwrap();
                let _stop = StopAtEnd(&signin);
                let mut shown = Vec::new();
                if let Some(code) = code {
                    wait_for(|| signin.phase() == Phase::CodeRequired, "the code request");
                    assert!(signin.submit_code(code));
                    shown.push(debug(&signin));
                }
                let result = signin.wait(LONG).unwrap();
                assert_eq!(result.ok, code == Some("123456"), "{scenario}");
                shown.extend([format!("{result:?}"), result.to_json().to_string(), debug(&signin)]);
                for event in events.lock().unwrap().iter() {
                    shown.extend([format!("{event:?}"), event.to_json().to_string()]);
                }
                let state = status(&scratch.web);
                shown.extend([format!("{state:?}"), state.to_json().to_string()]);
                assert!(shown.len() >= 8);
                for text in shown {
                    for typed in [username, password.as_str(), code.unwrap_or(password.as_str())] {
                        assert!(!text.contains(typed), "{scenario}: something typed came back");
                    }
                }
            }
        }

        #[test]
        fn what_is_typed_reaches_the_helper_unchanged() {
            let scratch = Scratch::with_route(&[]);
            let username = "søme.one+tag@example.com";
            let password = "pw-plain \"quoted\" \\ back\tslash\nsecond line {\"password\": \"x\"} é 🔑";
            let signin = android_signin(scratch.web.clone(), username, password, None).unwrap();
            assert!(signin.wait(LONG).unwrap().ok);
            let digest = |text: &str| {
                Sha256::digest(text.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect::<String>()
            };
            let report = scratch.report();
            assert_eq!(report["username_sha256"], digest(username));
            assert_eq!(report["password_sha256"], digest(password));
        }

        #[test]
        fn lines_for_the_helper_are_spelled_as_python_spells_them() {
            // json.dumps({"code": "123456"}) and json.dumps({"username": 'a"b', "password": "c\\d\n"})
            assert_eq!(json_line(&[("code", "123456")]), b"{\"code\": \"123456\"}\n");
            assert_eq!(
                json_line(&[("username", "a\"b"), ("password", "c\\d\n")]),
                b"{\"username\": \"a\\\"b\", \"password\": \"c\\\\d\\n\"}\n"
            );
            // one line whatever is typed, and the same text again when it is read as JSON
            let (username, password) = ("a\nb", "\u{0}\r\n\u{1f}\u{7f} é 🔑 \u{2028}");
            let tricky = json_line(&[("username", username), ("password", password)]);
            assert_eq!(tricky.iter().filter(|byte| **byte == b'\n').count(), 1);
            assert_eq!(tricky.last(), Some(&b'\n'));
            assert_eq!(
                serde_json::from_slice::<Value>(&tricky).unwrap(),
                json!({"username": username, "password": password})
            );
        }

        #[test]
        fn a_code_the_helper_does_not_take_is_a_notice_and_the_sign_in_goes_on() {
            let scratch = Scratch::with_route(&[]);
            let (events, listener) = recorder();
            let signin = android_signin(scratch.web.clone(), USERNAME, "pw-2fa", Some(listener)).unwrap();
            let _stop = StopAtEnd(&signin);
            wait_for(|| signin.phase() == Phase::CodeRequired, "the code request");
            assert!(signin.submit_code("12345"));
            let notice = SignInEvent::Notice { error: "The verification code is six digits".into() };
            wait_for(|| events.lock().unwrap().contains(&notice), "the notice");
            assert_eq!(signin.phase(), Phase::CodeRequired);
            assert!(signin.submit_code("123456"));
            assert!(signin.wait(LONG).unwrap().ok);
            assert_eq!(scratch.report()["codes"], 1); // only the second one was a code
        }

        #[test]
        fn a_refused_code_ends_the_attempt() {
            let scratch = Scratch::with_route(&[]);
            scratch.earlier_sign_in();
            let verdict = scratch.earlier_verdict();
            scratch.sign_in(&[Source::Browser]);
            scratch.web.demote_source(Source::Android);
            let signin = android_signin(scratch.web.clone(), USERNAME, "pw-2fa", None).unwrap();
            let _stop = StopAtEnd(&signin);
            wait_for(|| signin.phase() == Phase::CodeRequired, "the code request");
            assert!(signin.submit_code("000000"));
            let result = signin.wait(LONG).unwrap();
            assert_eq!((result.ok, result.kind.as_deref()), (false, Some("code")));
            assert_eq!(result.steps, ["signing_in", "code_prompt", "code_sent"]);
            // a failed attempt changes nothing here: the verdict and the order of the sign-ins stay
            assert!(verdict.exists());
            assert_eq!(scratch.web.token_sources(), [Source::Browser, Source::Android]);
            assert_eq!(scratch.account(), account_files("old"));
        }

        #[test]
        fn only_what_the_helper_may_say_is_passed_on() {
            let scratch = Scratch::with_route(&[]);
            let verdict = scratch.earlier_verdict();
            let (events, listener) = recorder();
            let signin = android_signin(scratch.web.clone(), USERNAME, "pw-noise", Some(listener)).unwrap();
            let result = signin.wait(LONG).unwrap();
            // the last result line counts; its fields are of the expected kind and bounded
            let mut steps = vec!["7".to_string()];
            steps.resize(40, "s".repeat(40));
            let expected = SignInResult { ok: true, kind: None, error: Some("e".repeat(600)), steps, log: None };
            assert_eq!(result, expected);
            assert_eq!(
                *events.lock().unwrap(),
                [
                    SignInEvent::Phase(Phase::Preparing),
                    SignInEvent::Phase(Phase::SigningIn),
                    SignInEvent::Notice { error: "p".repeat((1 << 20) - 33) }, // the longest line looked at
                    SignInEvent::Notice { error: "A notice".into() },
                    SignInEvent::Notice { error: String::new() },
                    SignInEvent::Phase(Phase::Verifying),
                    SignInEvent::Result(expected),
                ]
            );
            assert_eq!(signin.phase(), Phase::Verifying);
            assert!(!verdict.exists()); // a result that says ok is a new sign-in
        }

        #[test]
        fn a_helper_that_ends_without_a_result() {
            let scratch = Scratch::with_route(&[]);
            let verdict = scratch.earlier_verdict();
            let (events, listener) = recorder();
            let signin = android_signin(scratch.web.clone(), USERNAME, "pw-crash", Some(listener)).unwrap();
            assert_eq!(signin.wait(LONG), Some(stopped()));
            assert_eq!(events.lock().unwrap().last(), Some(&SignInEvent::Result(stopped())));
            assert!(!signin.running() && !signin.submit_code("123456"));
            assert!(verdict.exists());
        }

        #[test]
        fn the_result_arrives_once_the_helper_has_ended() {
            let scratch = Scratch::with_route(&[]);
            let cleaned_up = scratch.config.var.join("wrapper/cleaned-up");
            let at_the_result = Arc::new(Mutex::new(None));
            let (marker, seen) = (cleaned_up.clone(), at_the_result.clone());
            let listener: OnEvent = Box::new(move |event| {
                if let SignInEvent::Result(_) = event {
                    *seen.lock().unwrap() = Some(marker.exists());
                }
            });
            // this helper writes its result line, closes its output, cleans up for half a second and
            // leaves the marker
            let signin = android_signin(scratch.web.clone(), USERNAME, "pw-late", Some(listener)).unwrap();
            let result = signin.wait(LONG).unwrap();
            assert_eq!((result.kind.as_deref(), result.error.as_deref()), (Some("failed"), Some("Ended early")));
            assert_eq!(*at_the_result.lock().unwrap(), Some(true));
        }

        #[test]
        fn a_helper_that_cannot_be_started() {
            let error = start(Command::new("/nonexistent/am-ui-test/python3"), true).unwrap_err();
            assert_eq!(error, SessionError::HelperStart("NotFound".into()));
            assert_eq!(error.to_string(), "Could not start the sign-in helper (NotFound)");
        }

        #[test]
        fn a_helper_that_does_not_take_the_cancel_is_told_to_terminate() {
            let scratch = Scratch::with_route(&[]);
            // also what the helper has to end in once it was told to, before it would be killed:
            // long enough for a busy machine
            let grace = Duration::from_secs(1);
            let signin = start_signin(scratch.web.clone(), USERNAME, "pw-deaf", None, grace).unwrap();
            let _stop = StopAtEnd(&signin);
            wait_for(|| signin.phase() == Phase::SigningIn, "the sign-in to start");
            let started = Instant::now();
            signin.cancel();
            let result = signin.wait(LONG).unwrap();
            assert!(started.elapsed() >= grace);
            assert_eq!((result.ok, result.kind.as_deref()), (false, Some("cancelled"))); // its own clean cancel
        }

        #[test]
        fn a_helper_that_does_not_terminate_is_killed() {
            let scratch = Scratch::with_route(&[]);
            let grace = Duration::from_millis(300);
            let signin = start_signin(scratch.web.clone(), USERNAME, "pw-stubborn", None, grace).unwrap();
            let _stop = StopAtEnd(&signin);
            wait_for(|| signin.phase() == Phase::SigningIn, "the sign-in to start");
            let started = Instant::now();
            signin.cancel();
            assert_eq!(signin.wait(LONG), Some(stopped()));
            assert!(started.elapsed() >= 2 * grace);
            assert!(started.elapsed() < Duration::from_secs(20)); // stopped, not left to end by itself
        }

        #[test]
        fn a_helper_that_closed_its_output_is_still_waited_for_and_stopped() {
            let scratch = Scratch::with_route(&[]);
            let grace = Duration::from_millis(300);
            let signin = start_signin(scratch.web.clone(), USERNAME, "pw-linger", None, grace).unwrap();
            let _stop = StopAtEnd(&signin);
            wait_for(|| signin.phase() == Phase::SigningIn, "the sign-in to start");
            std::thread::sleep(Duration::from_millis(200));
            assert!(signin.running() && signin.result().is_none()); // it has not ended, only gone quiet
            let started = Instant::now();
            signin.cancel();
            assert_eq!(signin.wait(LONG), Some(stopped()));
            assert!(started.elapsed() >= grace);
        }
    }

    mod terminal_sign_in {
        //! `android_signin_terminal()` in a terminal (a pseudo-terminal here), the helper's own
        //! prompts answered there (TerminalSignInTest of tests/test_signin.py). The function looks
        //! at the standard streams of its process, so it runs in a process of its own: this test
        //! program again, asked for `driver` alone.
        use super::*;
        use std::ffi::CStr;
        use std::fs::File;
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;

        /// Names the scratch folder for `driver`.
        const DRIVER_ROOT: &str = "AMUI_TEST_SIGNIN_ROOT";

        /// Not a test of its own: the tests below start it and read what it prints.
        #[test]
        #[ignore = "run by the terminal sign-in tests in a process of its own"]
        fn driver() {
            let Some(root) = std::env::var_os(DRIVER_ROOT).map(PathBuf::from) else { return };
            // only over a scratch folder of these tests, where the sign-in helper is the stand-in
            let helper = fs::read_to_string(root.join("tools/lossless/wrapperctl.py")).unwrap_or_default();
            assert!(helper == STAND_IN, "{DRIVER_ROOT} does not name a scratch folder of these tests");
            let scratch = Scratch::over(root, &[]);
            scratch.web.demote_source(Source::Android);
            match android_signin_terminal(&scratch.web) {
                Ok(signed_in) => println!("RESULT {signed_in}"),
                Err(error) => println!("ERROR {error}"),
            }
            let verdict = scratch.config.android_session_status_file.exists();
            println!("VERDICT {}", if verdict { "kept" } else { "forgotten" });
            println!("FIRST {:?}", scratch.web.token_source());
            println!("REQUESTS {}", scratch.seen.lock().unwrap().len());
        }

        fn driver_command(scratch: &Scratch) -> Command {
            let module = module_path!().split_once("::").map_or("", |(_crate, module)| module);
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args(["--ignored", "--exact", &format!("{module}::driver"), "--nocapture"]);
            command.env(DRIVER_ROOT, &scratch.config.root);
            command
        }

        /// A driver that is stopped when its test is over, a failed one too.
        struct Driver(Child);

        impl Drop for Driver {
            fn drop(&mut self) {
                if let Ok(None) = self.0.try_wait() {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
        }

        /// Both ends of a new pseudo-terminal: the one this test types at, and the terminal.
        fn open_terminal() -> (File, File) {
            let open = |path: &str| {
                fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOCTTY).open(path).unwrap()
            };
            let control = open("/dev/ptmx");
            let mut name = [0; 64];
            // SAFETY: calls on a descriptor that is open, with a buffer of the length given, which
            // ptsname_r() ends with a NUL when it reports success.
            let name = unsafe {
                assert_eq!(libc::grantpt(control.as_raw_fd()), 0);
                assert_eq!(libc::unlockpt(control.as_raw_fd()), 0);
                assert_eq!(libc::ptsname_r(control.as_raw_fd(), name.as_mut_ptr(), name.len()), 0);
                CStr::from_ptr(name.as_ptr()).to_str().unwrap().to_owned()
            };
            let terminal = open(&name);
            (control, terminal)
        }

        fn readable(file: &File, within: Duration) -> bool {
            let mut wanted = libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one descriptor record that lives for the call.
            unsafe { libc::poll(&mut wanted, 1, within.as_millis() as libc::c_int) > 0 }
        }

        /// Run the driver on a pseudo-terminal; answers = (text to wait for, line to type).
        /// Returns its exit status and everything the terminal showed.
        fn run_in_terminal(scratch: &Scratch, answers: &[(&str, &str)]) -> (Option<i32>, String) {
            let (mut control, terminal) = open_terminal();
            let mut command = driver_command(scratch);
            command.stdin(terminal.try_clone().unwrap()).stdout(terminal.try_clone().unwrap()).stderr(terminal);
            // its own session: no controlling terminal, so the hidden prompt reads this pseudo-terminal
            // SAFETY: setsid() is async-signal-safe.
            unsafe {
                command.pre_exec(|| if libc::setsid() == -1 { Err(io::Error::last_os_error()) } else { Ok(()) });
            }
            let mut driver = Driver(command.spawn().unwrap());
            drop(command); // with it this process's handles on the terminal
            let mut output: Vec<u8> = Vec::new();
            let mut answered = 0; // the next answer waits for text that comes after this point
            let mut pending = answers.iter().peekable();
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut exited = false;
            while Instant::now() < deadline {
                if !readable(&control, Duration::from_millis(200)) {
                    // the driver may print its last lines and end between the two looks: only a
                    // quiet terminal after its end is the end of the output
                    if exited {
                        break;
                    }
                    exited = driver.0.try_wait().unwrap().is_some();
                    continue;
                }
                let mut chunk = [0; 4096];
                match control.read(&mut chunk) {
                    Ok(read) if read > 0 => output.extend_from_slice(&chunk[..read]),
                    _ => break, // the other end is closed
                }
                if let Some((prompt, answer)) = pending.peek()
                    && output[answered..].windows(prompt.len()).any(|window| window == prompt.as_bytes())
                {
                    control.write_all(format!("{answer}\n").as_bytes()).unwrap();
                    answered = output.len();
                    pending.next();
                }
            }
            let output = String::from_utf8_lossy(&output).into_owned();
            let ended = Instant::now() + Duration::from_secs(30);
            let status = loop {
                match driver.0.try_wait().unwrap() {
                    Some(status) => break status,
                    None => assert!(Instant::now() < ended, "the driver did not end: {output}"),
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            assert!(pending.peek().is_none(), "{output}");
            (status.code(), output)
        }

        #[test]
        fn sign_in_with_code() {
            let scratch = Scratch::with_route(&[]);
            scratch.earlier_sign_in();
            scratch.earlier_verdict();
            scratch.sign_in(&[Source::Browser]);
            let (code, output) = run_in_terminal(
                &scratch,
                &[("Apple account email:", USERNAME), ("(hidden):", "pw-2fa"), ("2FA code:", "123456")],
            );
            assert_eq!(code, Some(0), "{output}");
            assert!(output.contains("RESULT true"), "{output}");
            assert!(!output.contains("pw-2fa")); // the password is not echoed
            assert_eq!(scratch.account(), account_files("new"));
            assert!(!output.contains("new-MUSIC_TOKEN"));
            // the old verdict is forgotten and the new sign-in is used first, without a request
            assert!(output.contains("VERDICT forgotten") && output.contains("FIRST Some(Android)"), "{output}");
            assert!(output.contains("REQUESTS 0"), "{output}");
            // the helper ran in this terminal, started like the Python module starts it
            let report = scratch.report();
            assert_eq!(report["argv"], json!(["login", "--stop-service"]));
            assert_eq!(report["streams"], json!(["tty", "tty", "tty"]));
            assert_eq!((&report["isolated"], &report["umask"]), (&json!(true), &json!(0o077)));
            assert_eq!(report["typed_in_arguments_or_environment"], false);
        }

        #[test]
        fn failed_attempt_keeps_the_earlier_sign_in() {
            let scratch = Scratch::with_route(&[]);
            scratch.earlier_sign_in();
            scratch.earlier_verdict();
            scratch.sign_in(&[Source::Browser]);
            let (code, output) =
                run_in_terminal(&scratch, &[("Apple account email:", USERNAME), ("(hidden):", "pw-apple")]);
            assert_eq!(code, Some(0), "{output}");
            assert!(output.contains("RESULT false"), "{output}");
            assert_eq!(scratch.account()["MUSIC_TOKEN"], "old-MUSIC_TOKEN");
            assert!(!scratch.config.lossless_data_dir.join(".login-args").exists()); // nothing typed is left in a file
            // and nothing else changed: the verdict stays, the sign-ins keep their order
            assert!(output.contains("VERDICT kept") && output.contains("FIRST Some(Browser)"), "{output}");
        }

        #[test]
        fn needs_a_terminal() {
            let scratch = Scratch::with_route(&[]);
            let done = driver_command(&scratch).stdin(Stdio::null()).output().unwrap();
            let output = String::from_utf8_lossy(&done.stdout);
            assert!(done.status.success(), "{output}");
            assert!(output.contains(&format!("ERROR {}", SessionError::NoTerminal)), "{output}");
            assert!(output.contains("in a terminal"), "{output}");
            assert!(!scratch.report_file().exists()); // nothing was started
        }
    }
}
