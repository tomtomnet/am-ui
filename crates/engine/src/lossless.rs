//! Lossless (ALAC) acquisition through the local FairPlay wrapper service (tools/lossless).
//!
//! Apple serves ALAC only through FairPlay, to a client that proves it is an Apple Music device;
//! the Widevine license the web route uses for Dolby Atmos and AAC is refused for it (status
//! -1002). The wrapper-lite service (Apple's Android library stack, rootless, bound to 127.0.0.1;
//! built and signed in once with tools/lossless/{build,login}.sh, see research/lossless-route.md)
//! answers on loopback:
//!
//! ```text
//! GET /status                      {"regions": ["<storefront>"]}   empty list = not signed in
//! GET /m3u8?adamId=<song id>       {"m3u8": <the account's device master playlist URL>}  (plain CDN URL)
//! GET /key?adamId=&uri=skd://...   FairPlay decryption templates: used by the Go primitive only
//! ```
//!
//! Flow of `acquire_lossless()`: service check (started on demand through wrapperctl) -> /m3u8 ->
//! master playlist -> the ALAC rendition (highest sample rate <= `config.lossless_max_rate`, else
//! the lowest above it) -> `config.lossless_rip_helper` (download + decrypt + defragment + ALAC
//! end-marker repair; the rendition URL goes in on stdin, never on the command line) -> ffprobe
//! verification (alac, 2 channels, the rendition's sample rate) -> `MediaCache` key
//! `<id>.lossless` with the usual meta plus sample_rate/bit_depth.
//!
//! `acquire_hires()` is the same flow for the rendition above 48 kHz ([`select_hires`], up to
//! [`HIRES_MAX_RATE`]) under a key of its own, `<id>.hires`: the two files of a song never take
//! each other's place, and a `.hires` file is above 48 kHz because nothing else is stored there.
//!
//! Security: the wrapper's answers carry no account secret; playlist URLs may be signed and stay
//! in local variables (the primitive redacts URLs from its errors, this module never logs them).
//! Only HTTPS Apple CDN hosts are fetched (the HTTP layer's allowlist). Files are 0600 in 0700
//! directories.
//!
//! Failure model: [`LosslessError::Unavailable`] (service not built, not signed in, not
//! reachable; decryption failed), [`LosslessError::NoLossless`] (Apple offers no ALAC rendition
//! for this song) and [`LosslessError::NoHiRes`] (none above 48 kHz) reach callers as
//! [`AcquireError::Lossless`]: the player falls back to the next version down with a truthful
//! badge. Service failures arm a back-off (`available()` answers false for `BACKOFF_S`) so a dead
//! service does not cost every following track a start-up attempt. Failures of the HTTP layer
//! (rate limits, cancellation) pass through unchanged.
//!
//! Deliberate differences from `amui/lossless.py`:
//!
//! * What Python kept in module globals (the back-off, "this process started the service", the
//!   remembered `configured()` answer) lives in a [`Lossless`]; the engine holds one. Its
//!   functions are that type's methods. `helper_built()` and `signed_in_files()` only look at
//!   files and take the [`Config`].
//! * The two things the Python tests replaced are traits: [`Service`] (one GET to the loopback
//!   service) and [`Wrapperctl`] (`python3 -I <wrapperctl> <command>`). `Lossless::new()` uses the
//!   real ones, `Lossless::with_parts()` any.
//! * The loopback requests never use a proxy from the environment, follow no redirect, read at
//!   most 1 MiB and go to `http://127.0.0.1` only. An answer with an HTTP error status reads
//!   "does not answer (HTTP 500)" where Python printed "(str)", the type of urllib's reason.
//! * `service_status()` takes its timeout (Python defaulted to `STATUS_TIMEOUT_S`), and
//!   [`select_alac`] its limit; `Lossless::select_alac()` applies the configured one.
//! * `device_master()` refuses an id that is not numeric before it asks the service.
//! * A refusal text of the service has its URLs replaced by `<url>` like the helpers' messages.
//! * `rip()` creates the helper's log 0600 (Python: by the process's umask) and reads the
//!   helper's error output while it runs (Python read it afterwards, which a talkative helper
//!   would have blocked).
//! * `status()` is a JSON object; renditions are [`AlacVariant`]s whose `Debug` output leaves
//!   out the URL.
//! * The detached `wrapperctl stop` is waited for on a thread of its own, never by the caller.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::acquire::{
    AcquireError, Completed, LosslessRoute, ProgressFn, RunError, Staging, check_cancel, file_size, first_max,
    is_catalog_id, path_span, picked, probe_info, run_captured,
};
use crate::cache::MediaCache;
use crate::config::Config;
use crate::render::{os_error_name, private_umask, returncode, seconds, splitlines, terminate};
use crate::types::AcquireResult;
use crate::util::{self, Event};
use crate::web::{self, Http, WebError};

/// Presence and size only, never contents.
pub const ACCOUNT_FILES: [&str; 3] = ["MUSIC_TOKEN", "STOREFRONT_ID", "DEV_TOKEN"];
pub const STATUS_TIMEOUT_S: f64 = 2.0;
/// The wrapper's playback dispatch takes an Apple round trip.
pub const M3U8_TIMEOUT_S: f64 = 60.0;
pub const START_TIMEOUT_S: f64 = 90.0;
pub const RIP_TIMEOUT_S: f64 = 900.0;
pub const BACKOFF_S: f64 = 60.0;
pub const NO_LOSSLESS: &str = "Apple offers no lossless (ALAC) rendition for this song";
pub const NO_HIRES: &str = "Apple offers no Hi-Res Lossless rendition for this song";
/// Above this sample rate a rendition is Hi-Res Lossless (Apple's tiers).
pub const LOSSLESS_TOP_RATE: u32 = 48_000;
/// The highest sample rate Hi-Res Lossless fetches: the highest Apple offers.
pub const HIRES_MAX_RATE: u32 = 192_000;
/// More than any answer of the service needs.
const SERVICE_MAX_BYTES: u64 = 1024 * 1024;

/// The lossless route cannot deliver this song; the next version down is the truthful fallback.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LosslessError {
    /// The service is not built, not signed in, not reachable, or decryption failed.
    #[error("{0}")]
    Unavailable(String),
    /// Apple advertises no ALAC rendition for this song and account.
    #[error("{0}")]
    NoLossless(String),
    /// Apple advertises ALAC for this song, but none above 48 kHz.
    #[error("{0}")]
    NoHiRes(String),
}

impl LosslessError {
    /// The song does not have the rendition: the route itself is fine.
    pub fn is_no_rendition(&self) -> bool {
        matches!(self, LosslessError::NoLossless(_) | LosslessError::NoHiRes(_))
    }
}

/// One of the two ALAC versions of a song; each has its own entry in the media cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    /// Up to `config.lossless_max_rate`.
    Lossless,
    /// Above 48 kHz, up to [`HIRES_MAX_RATE`].
    HiRes,
}

impl Tier {
    /// The variant's name: the end of its cache key.
    fn variant(self) -> &'static str {
        match self {
            Tier::Lossless => "lossless",
            Tier::HiRes => "hires",
        }
    }
}

fn unavailable(message: impl Into<String>) -> LosslessError {
    LosslessError::Unavailable(message.into())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn pattern(cell: &'static OnceLock<Regex>, text: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(text).expect("a valid pattern"))
}

/// The text with every URL replaced by `<url>`.
fn redact(text: &str) -> String {
    static URL: OnceLock<Regex> = OnceLock::new();
    pattern(&URL, r"https?://\S+").replace_all(text, "<url>").into_owned()
}

fn clip(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// The last non-blank line a helper wrote to its error output, URLs redacted, at most `limit`
/// characters.
fn last_error_line(stderr: &str, limit: usize) -> Option<String> {
    let line = splitlines(stderr).into_iter().rev().find(|line| !line.trim().is_empty())?;
    Some(clip(&redact(line), limit))
}

// --- availability (cheap, filesystem only) ----------------------------------------

/// The rip helper and the control script are there.
pub fn helper_built(config: &Config) -> bool {
    config.lossless_rip_helper.is_file() && config.helper_present()
}

/// True when the one-time native sign-in left its account files (contents are never read).
pub fn signed_in_files(config: &Config) -> bool {
    ACCOUNT_FILES.iter().all(|name| {
        fs::metadata(config.lossless_data_dir.join(name)).is_ok_and(|info| info.is_file() && info.len() > 0)
    })
}

// --- the service -------------------------------------------------------------------

/// The wrapper service on loopback as this module asks it: one GET, the body of the answer, or
/// a short reason without addresses when there is none ("ConnectionRefusedError", "HTTP 500").
pub trait Service: Send + Sync {
    fn get(&self, path: &str, timeout: f64) -> Result<Vec<u8>, String>;
}

/// The real service: plain HTTP to `config.lossless_url` on 127.0.0.1, no credentials. This is
/// the one place where the engine opens a connection that is not Apple's and not through
/// `web::Http`.
pub struct LoopbackService {
    agent: ureq::Agent,
    base_url: String,
}

impl LoopbackService {
    pub fn new(base_url: &str) -> LoopbackService {
        let agent = ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .build()
            .into();
        LoopbackService { agent, base_url: base_url.to_string() }
    }
}

fn is_loopback(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|parsed| parsed.scheme() == "http" && parsed.host_str() == Some("127.0.0.1"))
}

/// Why a loopback request failed, in the words Python's exceptions have for it.
fn no_answer(error: ureq::Error) -> String {
    match error {
        ureq::Error::Timeout(_) => "TimeoutError".to_string(),
        ureq::Error::Io(error) => os_error_name(&error).to_string(),
        ureq::Error::ConnectionFailed => "ConnectionRefusedError".to_string(),
        ureq::Error::BodyExceedsLimit(_) => "answer too long".to_string(),
        _ => "OSError".to_string(),
    }
}

impl Service for LoopbackService {
    fn get(&self, path: &str, timeout: f64) -> Result<Vec<u8>, String> {
        let url = format!("{}{path}", self.base_url);
        if !is_loopback(&url) {
            return Err("not a loopback address".to_string());
        }
        let limit = Some(seconds(timeout));
        let request = self.agent.get(&url).config().timeout_global(limit).build();
        let response = request.call().map_err(no_answer)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}"));
        }
        response.into_body().with_config().limit(SERVICE_MAX_BYTES).read_to_vec().map_err(no_answer)
    }
}

/// The wrapper control helper (`Config::helper_command`): starts, checks and stops the wrapper
/// service.
pub trait Wrapperctl: Send + Sync {
    /// Run a command to its end, at most `timeout` seconds.
    fn run(&self, command: &str, timeout: f64) -> Result<Completed, RunError>;

    /// Start a command in a session of its own and leave it running.
    fn detach(&self, command: &str) -> io::Result<()>;
}

struct HelperWrapperctl {
    config: Arc<Config>,
}

impl HelperWrapperctl {
    fn python(&self, command: &str) -> Command {
        self.config.helper_command(&[command])
    }
}

impl Wrapperctl for HelperWrapperctl {
    fn run(&self, command: &str, timeout: f64) -> Result<Completed, RunError> {
        run_captured(&mut self.python(command), seconds(timeout))
    }

    fn detach(&self, command: &str) -> io::Result<()> {
        let mut python = self.python(command);
        python.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        // SAFETY: setsid() is async-signal-safe and only detaches the new process from this
        // one's session.
        unsafe {
            python.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = python.spawn()?;
        // nobody waits for it here; a thread of its own collects it when it ends
        let _ = thread::Builder::new().name("amui-wrapperctl".to_string()).spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

#[derive(Default)]
struct State {
    /// `available()` answers false until then.
    backoff_until: Option<Instant>,
    started_by_us: bool,
    /// When `configured()` last looked, and what it saw.
    configured: Option<(Instant, bool)>,
}

/// The lossless route of one engine: the service, the rip helper and the back-off.
pub struct Lossless {
    config: Arc<Config>,
    http: Arc<dyn Http>,
    service: Box<dyn Service>,
    control: Box<dyn Wrapperctl>,
    /// Held while the service is checked and started.
    starting: Mutex<()>,
    state: Mutex<State>,
}

impl Lossless {
    /// The route with the real service on `config.lossless_url` and the real control script.
    pub fn new(config: Arc<Config>, http: Arc<dyn Http>) -> Lossless {
        let service = Box::new(LoopbackService::new(&config.lossless_url));
        let control = Box::new(HelperWrapperctl { config: config.clone() });
        Lossless::with_parts(config, http, service, control)
    }

    /// The route with another service or control script (tests).
    pub fn with_parts(
        config: Arc<Config>,
        http: Arc<dyn Http>,
        service: Box<dyn Service>,
        control: Box<dyn Wrapperctl>,
    ) -> Lossless {
        Lossless { config, http, service, control, starting: Mutex::default(), state: Mutex::default() }
    }

    /// Built and signed in: the UI offers the lossless qualities when this is true. The filesystem
    /// answer is kept for two seconds (snapshots ask often).
    pub fn configured(&self) -> bool {
        let mut state = lock(&self.state);
        match state.configured {
            Some((asked, answer)) if asked.elapsed() <= Duration::from_secs(2) => answer,
            _ => {
                let answer = helper_built(&self.config) && signed_in_files(&self.config);
                state.configured = Some((Instant::now(), answer));
                answer
            }
        }
    }

    fn backoff_left(&self) -> Option<Duration> {
        lock(&self.state).backoff_until.and_then(|until| until.checked_duration_since(Instant::now()))
    }

    /// `configured()` and not inside the back-off window that follows a service failure.
    pub fn available(&self) -> bool {
        self.configured() && self.backoff_left().is_none()
    }

    /// Why the route cannot be used now, in a sentence for the user; `None` when it can. Only
    /// looks at files and the back-off.
    pub fn problem(&self) -> Option<String> {
        let problem = if !helper_built(&self.config) {
            "The lossless route is not built on this computer (./setup.sh builds it)"
        } else if !signed_in_files(&self.config) {
            "Lossless needs Apple's own sign-in: use Sign In"
        } else if self.backoff_left().is_some() {
            "The lossless service failed a moment ago; it is tried again shortly"
        } else {
            return None;
        };
        Some(problem.to_string())
    }

    /// A service failure: arms the back-off.
    fn fail(&self, message: impl Into<String>) -> LosslessError {
        lock(&self.state).backoff_until = Instant::now().checked_add(seconds(BACKOFF_S));
        unavailable(message)
    }

    pub fn clear_backoff(&self) {
        let mut state = lock(&self.state);
        state.backoff_until = None;
        state.configured = None;
    }

    /// GET `config.lossless_url` + path (loopback, no credentials) and unwrap the
    /// {"code","msg","data"} envelope.
    fn get_json(&self, path: &str, timeout: f64) -> Result<Map<String, Value>, LosslessError> {
        let silent = |reason: &str| unavailable(format!("the lossless service does not answer ({reason})"));
        let body = self.service.get(path, timeout).map_err(|reason| silent(&reason))?;
        let payload: Value = serde_json::from_slice(&body).map_err(|_| silent("JSONDecodeError"))?;
        let Value::Object(mut payload) = payload else {
            return Err(unavailable("the lossless service returned an unexpected answer"));
        };
        let accepted = match payload.get("code") {
            Some(Value::Number(code)) => code.as_f64() == Some(0.0),
            Some(Value::Bool(flag)) => !flag,
            _ => false,
        };
        if !accepted {
            let message = match payload.get("msg") {
                Some(Value::String(text)) if !text.is_empty() => text.clone(),
                Some(Value::Number(number)) if number.as_f64() != Some(0.0) => number.to_string(),
                _ => "error".to_string(),
            };
            return Err(unavailable(format!("the lossless service refused: {}", clip(&redact(&message), 120))));
        }
        match payload.remove("data") {
            Some(Value::Object(data)) => Ok(data),
            _ => Ok(Map::new()),
        }
    }

    /// Storefront ids the running service is signed in to; empty when running but signed out;
    /// `None` when nothing answers within `timeout` seconds.
    pub fn service_status(&self, timeout: f64) -> Option<Vec<String>> {
        let data = self.get_json("/status", timeout).ok()?;
        let Some(Value::Array(regions)) = data.get("regions") else { return Some(Vec::new()) };
        let text = |region: &Value| region.as_str().map_or_else(|| region.to_string(), str::to_owned);
        Some(regions.iter().map(text).collect())
    }

    fn wrapperctl(&self, command: &str, timeout: f64) -> Result<Completed, LosslessError> {
        self.control.run(command, timeout).map_err(|error| match error {
            RunError::Start(error) => {
                self.fail(format!("could not run the lossless helper ({})", os_error_name(&error)))
            }
            RunError::TimedOut => self.fail(format!("the lossless helper '{command}' timed out")),
        })
    }

    /// Return the signed-in storefronts of a running service, starting it when needed
    /// (idempotent).
    pub fn ensure_service(&self, mut progress: Option<ProgressFn<'_>>) -> Result<Vec<String>, LosslessError> {
        if !helper_built(&self.config) {
            return Err(unavailable("the lossless route is not built (tools/lossless/build.sh)"));
        }
        let _starting = lock(&self.starting);
        match self.service_status(STATUS_TIMEOUT_S) {
            Some(regions) if !regions.is_empty() => return Ok(regions),
            Some(_) => {
                return Err(self.fail("the lossless service is running but not signed in: sign in with Connect"));
            }
            None => {}
        }
        if !signed_in_files(&self.config) {
            return Err(unavailable("lossless needs Apple's native sign-in: sign in with Connect"));
        }
        if let Some(callback) = progress.as_mut() {
            callback("service", None);
        }
        let completed = self.wrapperctl("start", START_TIMEOUT_S)?;
        if completed.returncode != 0 {
            let reason = last_error_line(&completed.stderr, 160).unwrap_or(format!("exit {}", completed.returncode));
            return Err(self.fail(format!("could not start the lossless service: {reason}")));
        }
        lock(&self.state).started_by_us = true;
        match self.service_status(5.0) {
            Some(regions) if !regions.is_empty() => Ok(regions),
            _ => Err(self.fail("the lossless service started but reports no signed-in account")),
        }
    }

    /// At shutdown: stop the service only if this engine started it (detached; never waits).
    pub fn stop_service_if_started(&self) {
        {
            let mut state = lock(&self.state);
            if !state.started_by_us || !self.config.helper_present() {
                return;
            }
            state.started_by_us = false;
        }
        let _ = self.control.detach("stop");
    }

    /// (master playlist text, master URL) from the account's device playback dispatch.
    ///
    /// For a song without a lossless rendition the dispatch does not name a playlist: it names
    /// the song's AAC media file (….m4p). That answer is [`LosslessError::NoLossless`], and the
    /// file is not fetched.
    pub fn device_master(&self, catalog_id: &str) -> Result<(String, String), AcquireError> {
        if !is_catalog_id(catalog_id) {
            return Err(AcquireError::other("catalog id must be numeric"));
        }
        let data = self.get_json(&format!("/m3u8?adamId={catalog_id}"), M3U8_TIMEOUT_S)?;
        let Some(master_url) = data.get("m3u8").and_then(Value::as_str).filter(|url| !url.is_empty()) else {
            return Err(unavailable("the lossless service returned no playlist for this song").into());
        };
        let (start, end) = path_span(master_url);
        if !master_url[start..end].to_lowercase().ends_with(".m3u8") {
            return Err(LosslessError::NoLossless(NO_LOSSLESS.to_string()).into());
        }
        web::validate_url(master_url, false)?;
        match self.http.get_text(master_url) {
            Ok(master) => Ok((master, master_url.to_string())),
            // named like a playlist, but not one
            Err(WebError::NotText(_)) => Err(LosslessError::NoLossless(NO_LOSSLESS.to_string()).into()),
            Err(error) => Err(error.into()),
        }
    }

    /// The ALAC rendition to fetch under the configured limit (`config.lossless_max_rate`); see
    /// [`select_alac`].
    pub fn select_alac(&self, master: &str, master_url: &str) -> Result<AlacVariant, LosslessError> {
        select_alac(master, master_url, self.config.lossless_max_rate)
    }

    // --- the Go primitive

    /// Run the primitive: `variant_url` on stdin, progress to `log_path`, a finished .m4a at
    /// `out_path`. Returns the output size; fails with [`LosslessError::Unavailable`] (service
    /// or decrypt failure) or `WebError::Cancelled`.
    pub fn rip(
        &self,
        catalog_id: &str,
        variant_url: &str,
        out_path: &Path,
        log_path: &Path,
        cancel: Option<&Event>,
        timeout: f64,
    ) -> Result<u64, AcquireError> {
        let helper = &self.config.lossless_rip_helper;
        if !helper.is_file() {
            return Err(unavailable("the lossless route is not built (tools/lossless/build.sh)").into());
        }
        let _ = fs::remove_file(out_path);
        let start = || -> io::Result<Child> {
            let log = OpenOptions::new().append(true).create(true).mode(0o600).open(log_path)?;
            let mut program = Command::new(helper);
            program
                .arg("--lite")
                .arg(&self.config.lossless_url)
                .arg("--adam")
                .arg(catalog_id)
                .arg("--out")
                .arg(out_path);
            program.stdin(Stdio::piped()).stdout(log).stderr(Stdio::piped());
            private_umask(&mut program);
            program.spawn()
        };
        let mut process = start().map_err(|error| {
            self.fail(format!("could not run the lossless decrypt helper ({})", os_error_name(&error)))
        })?;
        if let Some(mut stdin) = process.stdin.take() {
            // when the helper exited early this fails, and its error output explains
            let _ = stdin.write_all(format!("{variant_url}\n").as_bytes());
        }
        let errors = process.stderr.take().and_then(|mut pipe| {
            let read = move || {
                let mut text = Vec::new();
                let _ = pipe.read_to_end(&mut text);
                text
            };
            thread::Builder::new().name("amui-rip-errors".to_string()).spawn(read).ok()
        });
        let deadline = Instant::now().checked_add(seconds(timeout));
        let status = loop {
            match process.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(_) => {
                    terminate(&mut process, Duration::ZERO);
                    return Err(self.fail("the lossless helper could not be watched").into());
                }
            }
            if cancel.is_some_and(Event::is_set) {
                terminate(&mut process, Duration::from_secs(5));
                return Err(WebError::Cancelled("cancelled".to_string()).into());
            }
            if deadline.is_some_and(|deadline| Instant::now() > deadline) {
                let _ = process.kill();
                let _ = process.wait();
                return Err(self.fail("the lossless download timed out").into());
            }
            thread::sleep(Duration::from_millis(50));
        };
        let stderr = errors.and_then(|reader| reader.join().ok()).unwrap_or_default();
        let code = returncode(status);
        if code != 0 {
            let reason =
                last_error_line(&String::from_utf8_lossy(&stderr), 200).unwrap_or(format!("exit status {code}"));
            let _ = fs::remove_file(out_path);
            if reason.contains("already exists") || reason.contains("must") {
                // a local bug, no back-off
                return Err(unavailable(format!("lossless helper refused: {reason}")).into());
            }
            return Err(self.fail(format!("lossless decryption failed: {reason}")).into());
        }
        match fs::metadata(out_path) {
            Ok(info) if info.is_file() && info.len() > 0 => Ok(info.len()),
            _ => Err(self.fail("the lossless helper reported success but wrote no output").into()),
        }
    }

    // --- the whole flow

    /// Cache key `<id>.lossless`. Phases: "service" (only when the wrapper has to start),
    /// "manifest", "download", "verify", "done". See the module text for the failure model.
    pub fn acquire_lossless(
        &self,
        catalog_id: &str,
        media_cache: &MediaCache,
        progress: Option<ProgressFn<'_>>,
        cancel: Option<&Event>,
    ) -> Result<AcquireResult, AcquireError> {
        self.acquire_tier(Tier::Lossless, catalog_id, media_cache, progress, cancel)
    }

    /// The rendition above 48 kHz: `acquire_lossless()` under the cache key `<id>.hires`.
    /// [`LosslessError::NoHiRes`] when Apple offers ALAC for the song but none above 48 kHz.
    pub fn acquire_hires(
        &self,
        catalog_id: &str,
        media_cache: &MediaCache,
        progress: Option<ProgressFn<'_>>,
        cancel: Option<&Event>,
    ) -> Result<AcquireResult, AcquireError> {
        self.acquire_tier(Tier::HiRes, catalog_id, media_cache, progress, cancel)
    }

    fn acquire_tier(
        &self,
        tier: Tier,
        catalog_id: &str,
        media_cache: &MediaCache,
        mut progress: Option<ProgressFn<'_>>,
        cancel: Option<&Event>,
    ) -> Result<AcquireResult, AcquireError> {
        if !is_catalog_id(catalog_id) {
            return Err(AcquireError::other("catalog id must be numeric"));
        }
        let key = format!("{catalog_id}.{}", tier.variant());
        let mut report = |phase: &str, fraction: Option<f64>| {
            if let Some(callback) = progress.as_mut() {
                callback(phase, fraction);
            }
        };
        let cancelled = || check_cancel(cancel, "cancelled");

        if let Some(cached) = media_cache.get(&key)? {
            let meta = media_cache.meta(&key)?.unwrap_or_default();
            report("done", Some(1.0));
            return Ok(AcquireResult {
                catalog_id: catalog_id.to_string(),
                bytes: file_size(&cached)?,
                path: cached.to_string_lossy().into_owned(),
                variant: variant_meta(&meta),
                duration_s: meta.get("duration_s").and_then(Value::as_f64),
                from_cache: true,
            });
        }

        cancelled()?;
        if !self.configured() {
            let reason = if helper_built(&self.config) {
                "lossless needs Apple's native sign-in: sign in with Connect"
            } else {
                "the lossless route is not built (./setup.sh builds it)"
            };
            return Err(unavailable(reason).into());
        }
        if self.backoff_left().is_some() {
            return Err(unavailable("the lossless service failed a moment ago; trying again shortly").into());
        }
        self.ensure_service(Some(&mut report))?;
        cancelled()?;
        report("manifest", None);
        let (master, master_url) = self.device_master(catalog_id)?;
        let selected = match tier {
            Tier::Lossless => self.select_alac(&master, &master_url)?,
            Tier::HiRes => select_hires(&master, &master_url)?,
        };
        drop(master);
        cancelled()?;

        let output = media_cache.staging_path(&key, ".decrypted.m4a")?;
        let log_path = media_cache.staging_path(&key, ".rip.log")?;
        let _staging = Staging(vec![log_path.clone(), output.clone()]);
        report("download", None);
        let size = self.rip(catalog_id, &selected.url, &output, &log_path, cancel, RIP_TIMEOUT_S)?;
        cancelled()?;
        report("verify", None);
        let info = probe_info(&output);
        if info.codec.as_deref() != Some("alac") || info.channels.unwrap_or(0) != 2 {
            let codec = info.codec.as_deref().unwrap_or("None");
            let channels = info.channels.map_or("None".to_string(), |channels| channels.to_string());
            return Err(self.fail(format!("the decrypted file is not stereo ALAC ({codec}, {channels} ch)")).into());
        }
        if info.sample_rate.is_some_and(|rate| rate != 0 && rate != i64::from(selected.sample_rate)) {
            return Err(self.fail("the decrypted file's sample rate does not match the rendition").into());
        }
        let mut meta = Map::new();
        meta.insert("catalog_id".to_string(), json!(catalog_id));
        meta.insert("variant".to_string(), json!(tier.variant()));
        meta.insert("audio_group".to_string(), json!(selected.audio_group));
        meta.insert("codecs".to_string(), json!(selected.codecs));
        meta.insert("bandwidth".to_string(), json!(selected.bandwidth));
        meta.insert("bytes".to_string(), json!(size));
        meta.insert("acquired_at".to_string(), json!(util::unix_now()));
        meta.extend(info.to_map());
        meta.insert("sample_rate".to_string(), json!(selected.sample_rate));
        meta.insert("bit_depth".to_string(), json!(selected.bit_depth));
        let dest = media_cache.put(&key, &output, &meta)?;
        self.clear_backoff();
        report("done", Some(1.0));
        Ok(AcquireResult {
            catalog_id: catalog_id.to_string(),
            path: dest.to_string_lossy().into_owned(),
            bytes: size,
            variant: variant_meta(&meta),
            duration_s: info.duration_s,
            from_cache: false,
        })
    }

    /// Secret-free summary for the command line and the UI: built, signed in, running,
    /// storefronts, back-off.
    pub fn status(&self) -> Map<String, Value> {
        let built = helper_built(&self.config);
        let regions = if built { self.service_status(STATUS_TIMEOUT_S) } else { None };
        let backoff = self.backoff_left().map_or(0.0, |left| (left.as_secs_f64() * 10.0).round() / 10.0);
        let mut state = Map::new();
        state.insert("built".to_string(), json!(built));
        state.insert("signed_in_files".to_string(), json!(signed_in_files(&self.config)));
        state.insert("running".to_string(), json!(regions.is_some()));
        state.insert("storefronts".to_string(), json!(regions.unwrap_or_default()));
        state.insert("available".to_string(), json!(self.available()));
        state.insert("backoff_s".to_string(), json!(backoff));
        state.insert("max_sample_rate".to_string(), json!(self.config.lossless_max_rate));
        state.insert("url".to_string(), json!(self.config.lossless_url));
        state
    }
}

impl LosslessRoute for Lossless {
    fn acquire_lossless(
        &self,
        catalog_id: &str,
        media_cache: &MediaCache,
        progress: Option<ProgressFn<'_>>,
        cancel: Option<&Event>,
    ) -> Result<AcquireResult, AcquireError> {
        Lossless::acquire_lossless(self, catalog_id, media_cache, progress, cancel)
    }

    fn acquire_hires(
        &self,
        catalog_id: &str,
        media_cache: &MediaCache,
        progress: Option<ProgressFn<'_>>,
        cancel: Option<&Event>,
    ) -> Result<AcquireResult, AcquireError> {
        Lossless::acquire_hires(self, catalog_id, media_cache, progress, cancel)
    }
}

fn variant_meta(meta: &Map<String, Value>) -> Map<String, Value> {
    picked(meta, &["audio_group", "codecs", "bandwidth", "sample_rate", "bit_depth"])
}

// --- pure helpers (unit-tested) ----------------------------------------------------

/// One ALAC rendition of a device master playlist.
#[derive(Clone, PartialEq, Eq)]
pub struct AlacVariant {
    /// The rendition's playlist. It may be signed: it stays out of logs and errors.
    pub url: String,
    /// `audio-alac-stereo-<sample rate>-<bit depth>`
    pub audio_group: String,
    pub codecs: String,
    /// `AVERAGE-BANDWIDTH`, else `BANDWIDTH`, as the playlist spells it.
    pub bandwidth: String,
    pub sample_rate: u32,
    pub bit_depth: u32,
}

impl std::fmt::Debug for AlacVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlacVariant")
            .field("audio_group", &self.audio_group)
            .field("bandwidth", &self.bandwidth)
            .finish_non_exhaustive()
    }
}

/// Every `#EXT-X-STREAM-INF` with `CODECS` alac in an `audio-alac-stereo-<rate>-<depth>` group.
pub fn alac_variants(master: &str, master_url: &str) -> Vec<AlacVariant> {
    static ATTRIBUTE: OnceLock<Regex> = OnceLock::new();
    static GROUP: OnceLock<Regex> = OnceLock::new();
    let attribute = pattern(&ATTRIBUTE, r#"([A-Z0-9-]+)=("[^"]*"|[^,]*)"#);
    let rates = pattern(&GROUP, r"audio-alac-stereo-(\d+)-(\d+)$");
    let lines = splitlines(master);
    let mut found = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(tag) = line.strip_prefix("#EXT-X-STREAM-INF:") else { continue };
        let attrs: HashMap<&str, &str> = attribute
            .captures_iter(tag)
            .filter_map(|found| Some((found.get(1)?.as_str(), found.get(2)?.as_str())))
            .collect();
        let unquoted = |name: &str| attrs.get(name).map_or("", |value| value.trim_matches('"'));
        let (codecs, audio_group) = (unquoted("CODECS"), unquoted("AUDIO"));
        if codecs != "alac" {
            continue;
        }
        let Some(rate) = rates.captures(audio_group) else { continue };
        let (Ok(sample_rate), Ok(bit_depth)) = (rate[1].parse(), rate[2].parse()) else { continue };
        let following = lines.iter().skip(index + 1).take(2);
        let Some(uri) =
            following.filter(|line| !line.starts_with('#')).map(|line| line.trim()).find(|line| !line.is_empty())
        else {
            continue;
        };
        let bandwidth = ["AVERAGE-BANDWIDTH", "BANDWIDTH"]
            .iter()
            .filter_map(|name| attrs.get(name))
            .find(|value| !value.is_empty())
            .map_or("0", |value| value.trim_matches('"'));
        found.push(AlacVariant {
            url: util::urljoin(master_url, uri).unwrap_or_else(|| uri.to_string()),
            audio_group: audio_group.to_string(),
            codecs: codecs.to_string(),
            bandwidth: bandwidth.to_string(),
            sample_rate,
            bit_depth,
        });
    }
    found
}

/// The ALAC rendition to fetch: the highest sample rate not above `max_rate`; when Apple offers
/// nothing at or below it, the lowest rate above. Ties go to the higher bit depth, then the
/// higher bandwidth. [`LosslessError::NoLossless`] when there is none.
pub fn select_alac(master: &str, master_url: &str, max_rate: u32) -> Result<AlacVariant, LosslessError> {
    let variants = alac_variants(master, master_url);
    let bandwidth = |variant: &AlacVariant| variant.bandwidth.trim().parse::<i64>().unwrap_or(0);
    let within: Vec<&AlacVariant> = variants.iter().filter(|variant| variant.sample_rate <= max_rate).collect();
    let chosen = if within.is_empty() {
        first_max(variants.iter(), |variant| {
            (std::cmp::Reverse(variant.sample_rate), variant.bit_depth, bandwidth(variant))
        })
    } else {
        first_max(within, |variant| (variant.sample_rate, variant.bit_depth, bandwidth(variant)))
    };
    chosen.cloned().ok_or_else(|| LosslessError::NoLossless(NO_LOSSLESS.to_string()))
}

/// The Hi-Res Lossless rendition to fetch: the highest sample rate above 48 kHz, up to
/// [`HIRES_MAX_RATE`]. Ties go to the higher bit depth, then the higher bandwidth.
/// [`LosslessError::NoHiRes`] when the song's ALAC goes no higher than 48 kHz,
/// [`LosslessError::NoLossless`] when it has none.
pub fn select_hires(master: &str, master_url: &str) -> Result<AlacVariant, LosslessError> {
    let variants = alac_variants(master, master_url);
    let bandwidth = |variant: &AlacVariant| variant.bandwidth.trim().parse::<i64>().unwrap_or(0);
    let hires =
        variants.iter().filter(|variant| (LOSSLESS_TOP_RATE + 1..=HIRES_MAX_RATE).contains(&variant.sample_rate));
    match first_max(hires, |variant| (variant.sample_rate, variant.bit_depth, bandwidth(variant))) {
        Some(chosen) => Ok(chosen.clone()),
        None if variants.is_empty() => Err(LosslessError::NoLossless(NO_LOSSLESS.to_string())),
        None => Err(LosslessError::NoHiRes(NO_HIRES.to_string())),
    }
}

/// A number as Python's `{:g}` prints it: six significant digits, no trailing zeros.
fn general(value: f64) -> String {
    let scientific = format!("{value:.5e}");
    let Some((mantissa, exponent)) = scientific.split_once('e') else { return scientific };
    let Ok(exponent) = exponent.parse::<i32>() else { return scientific };
    let trimmed = |digits: &str| match digits.contains('.') {
        true => digits.trim_end_matches('0').trim_end_matches('.').to_string(),
        false => digits.to_string(),
    };
    if !(-4..6).contains(&exponent) {
        let sign = if exponent < 0 { '-' } else { '+' };
        return format!("{}e{sign}{:02}", trimmed(mantissa), exponent.abs());
    }
    let decimals = usize::try_from(5 - exponent).unwrap_or(0);
    trimmed(&format!("{value:.decimals$}"))
}

/// "Lossless · ALAC 24-bit/48 kHz", "Hi-Res Lossless · ALAC 24-bit/96 kHz" (Apple's tier names).
pub fn quality_label(sample_rate: Option<u32>, bit_depth: Option<u32>) -> String {
    let sample_rate = sample_rate.filter(|rate| *rate != 0);
    let tier = if sample_rate.is_some_and(|rate| rate > LOSSLESS_TOP_RATE) { "Hi-Res Lossless" } else { "Lossless" };
    let mut parts = Vec::new();
    if let Some(depth) = bit_depth.filter(|depth| *depth != 0) {
        parts.push(format!("{depth}-bit"));
    }
    if let Some(rate) = sample_rate {
        parts.push(format!("{} kHz", general(f64::from(rate) / 1000.0)));
    }
    if parts.is_empty() { format!("{tier} · ALAC") } else { format!("{tier} · ALAC {}", parts.join("/")) }
}

#[cfg(test)]
mod tests {
    //! Rendition selection, the acquire flow around a stand-in rip helper, the failure model.
    //!
    //! No network, no wrapper, no credentials: the service and the control script are stand-ins;
    //! the "rip" is a shell script that copies a tiny ALAC file ffmpeg produced (those tests are
    //! skipped when ffmpeg is missing).
    use super::*;
    use crate::acquire::testing::{FakeHttp, files_in, scratch, tone, write_program};
    use crate::acquire::{Acquirer, decode_stereo};
    use crate::config;
    use std::io::{BufRead, BufReader};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    const MASTER_URL: &str = "https://aod.itunes.apple.com/itunes-assets/HLSMusic/v4/ab/cd/P1000_default.m3u8";
    const MEDIA_URL: &str =
        "https://streamingaudio.itunes.apple.com/itunes-assets/Music/v4/ab/cd/mzaf_1000.aac.a.m4p?accessKey=k";
    /// A made-up storefront id.
    const STOREFRONT: &str = "100000";

    fn master(variants: &[(&str, &str, i64)]) -> String {
        let mut lines: Vec<String> =
            vec!["#EXTM3U".into(), "#EXT-X-VERSION:7".into(), "#EXT-X-INDEPENDENT-SEGMENTS".into()];
        for (group, _, _) in variants {
            lines.push(format!(
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"{group}\",AUTOSELECT=YES,NAME=\"songEnhanced\",CHANNELS=\"2\""
            ));
        }
        for (group, codecs, bandwidth) in variants {
            lines.push(format!(
                "#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH={},BANDWIDTH={bandwidth},CODECS=\"{codecs}\",\
                 STABLE-VARIANT-ID=\"id-{group}\",AUDIO=\"{group}\"",
                bandwidth - 100
            ));
            lines.push(format!("{group}/P1000_default.m3u8"));
        }
        lines.join("\n") + "\n"
    }

    fn device_master() -> String {
        master(&[
            ("audio-stereo-256", "mp4a.40.2", 270717),
            ("audio-atmos-2768", "ec-3", 772543),
            ("audio-alac-stereo-44100-16", "alac", 900000),
            ("audio-alac-stereo-48000-24", "alac", 1995449),
            ("audio-alac-stereo-96000-24", "alac", 3800000),
            ("audio-alac-stereo-192000-24", "alac", 7000000),
            ("audio-HE-stereo-64", "mp4a.40.5", 78175),
        ])
    }

    // --- selection

    #[test]
    fn variants_are_parsed_with_rate_and_depth() {
        let variants = alac_variants(&device_master(), MASTER_URL);
        assert_eq!(variants.iter().map(|v| v.sample_rate).collect::<Vec<_>>(), [44100, 48000, 96000, 192000]);
        assert_eq!(variants.iter().map(|v| v.bit_depth).collect::<Vec<_>>(), [16, 24, 24, 24]);
        assert_eq!(
            variants[1].url,
            "https://aod.itunes.apple.com/itunes-assets/HLSMusic/v4/ab/cd/audio-alac-stereo-48000-24/P1000_default.m3u8"
        );
        assert_eq!(variants[1].bandwidth, "1995349");
        assert_eq!(variants[1].codecs, "alac");
    }

    #[test]
    fn select_prefers_the_highest_rate_within_the_limit() {
        let group = |limit: u32| select_alac(&device_master(), MASTER_URL, limit).unwrap().audio_group;
        assert_eq!(group(48000), "audio-alac-stereo-48000-24");
        assert_eq!(group(96000), "audio-alac-stereo-96000-24");
        assert_eq!(group(192000), "audio-alac-stereo-192000-24");
        assert_eq!(group(44100), "audio-alac-stereo-44100-16");
    }

    #[test]
    fn select_takes_the_lowest_rate_above_the_limit_when_nothing_fits() {
        let hires_only = master(&[
            ("audio-alac-stereo-96000-24", "alac", 3800000),
            ("audio-alac-stereo-192000-24", "alac", 7000000),
        ]);
        assert_eq!(select_alac(&hires_only, MASTER_URL, 48000).unwrap().sample_rate, 96000);
    }

    #[test]
    fn select_uses_the_configured_default_limit() {
        let fixture = Fixture::with_config(|config| config.lossless_max_rate = 96000);
        assert_eq!(fixture.lossless.select_alac(&device_master(), MASTER_URL).unwrap().sample_rate, 96000);
        let fixture = Fixture::new();
        assert_eq!(fixture.lossless.select_alac(&device_master(), MASTER_URL).unwrap().sample_rate, 48000);
    }

    #[test]
    fn hires_takes_the_highest_rendition_above_48_khz() {
        let chosen = select_hires(&device_master(), MASTER_URL).unwrap();
        assert_eq!((chosen.sample_rate, chosen.bit_depth), (192000, 24));
        assert_eq!(chosen.audio_group, "audio-alac-stereo-192000-24");
        // ... and only those: 96 kHz when that is the top, nothing beyond what Apple's tier has
        let up_to_96 = master(&[
            ("audio-alac-stereo-48000-24", "alac", 1995449),
            ("audio-alac-stereo-96000-24", "alac", 3800000),
            ("audio-alac-stereo-384000-24", "alac", 14000000),
        ]);
        assert_eq!(select_hires(&up_to_96, MASTER_URL).unwrap().sample_rate, 96000);
        // the same master as the Lossless quality reads it: the two never pick the same file
        assert_eq!(select_alac(&up_to_96, MASTER_URL, 48000).unwrap().sample_rate, 48000);
    }

    #[test]
    fn a_song_without_a_rendition_above_48_khz_has_no_hires() {
        let plain =
            master(&[("audio-alac-stereo-44100-16", "alac", 900000), ("audio-alac-stereo-48000-24", "alac", 1995449)]);
        let error = select_hires(&plain, MASTER_URL).unwrap_err();
        assert_eq!(error, LosslessError::NoHiRes(NO_HIRES.to_string()));
        assert!(error.is_no_rendition());
        // without any ALAC the answer is the wider one: nothing lossless is worth trying either
        let lossy = master(&[("audio-stereo-256", "mp4a.40.2", 270717)]);
        assert_eq!(select_hires(&lossy, MASTER_URL).unwrap_err(), LosslessError::NoLossless(NO_LOSSLESS.to_string()));
        assert!(!LosslessError::Unavailable("gone".to_string()).is_no_rendition());
    }

    #[test]
    fn no_alac_raises_no_lossless() {
        let lossy = master(&[("audio-stereo-256", "mp4a.40.2", 270717), ("audio-atmos-2768", "ec-3", 772543)]);
        let error = select_alac(&lossy, MASTER_URL, 48000).unwrap_err();
        assert!(matches!(error, LosslessError::NoLossless(_)));
        // every error of this route is an acquisition error
        let error = AcquireError::from(error);
        assert!(error.is_lossless() && !error.is_no_atmos());
        assert_eq!(error.to_string(), NO_LOSSLESS);
    }

    #[test]
    fn quality_labels() {
        assert_eq!(quality_label(Some(48000), Some(24)), "Lossless · ALAC 24-bit/48 kHz");
        assert_eq!(quality_label(Some(44100), Some(16)), "Lossless · ALAC 16-bit/44.1 kHz");
        assert_eq!(quality_label(Some(192000), Some(24)), "Hi-Res Lossless · ALAC 24-bit/192 kHz");
        assert_eq!(quality_label(None, None), "Lossless · ALAC");
    }

    // --- stand-ins for the service and the control script

    fn envelope(data: Value) -> Vec<u8> {
        json!({"code": 0, "msg": "SUCCESS", "data": data}).to_string().into_bytes()
    }

    /// What the stand-in service answers; tests change it as they go.
    struct Script {
        /// Storefronts `/status` reports; `None`: nothing answers.
        regions: Option<Vec<String>>,
        /// The "data" of `/m3u8`.
        dispatch: Value,
        /// What the device master URL holds.
        master: Result<Vec<u8>, WebError>,
        /// What `wrapperctl start` leaves, and the storefronts the service reports afterwards.
        start: Result<(i32, String), &'static str>,
        started_regions: Option<Vec<String>>,
    }

    #[derive(Clone)]
    struct Stage {
        script: Arc<Mutex<Script>>,
        service_calls: Arc<Mutex<Vec<String>>>,
        control_calls: Arc<Mutex<Vec<String>>>,
    }

    impl Service for Stage {
        fn get(&self, path: &str, _timeout: f64) -> Result<Vec<u8>, String> {
            self.service_calls.lock().unwrap().push(path.to_string());
            let script = self.script.lock().unwrap();
            if path != "/status" {
                return Ok(envelope(script.dispatch.clone()));
            }
            match &script.regions {
                Some(regions) => Ok(envelope(json!({ "regions": regions }))),
                None => Err("ConnectionRefusedError".to_string()),
            }
        }
    }

    impl Wrapperctl for Stage {
        fn run(&self, command: &str, _timeout: f64) -> Result<Completed, RunError> {
            self.control_calls.lock().unwrap().push(command.to_string());
            let mut script = self.script.lock().unwrap();
            match script.start.clone() {
                Ok((returncode, stderr)) => {
                    if returncode == 0 {
                        script.regions = script.started_regions.clone();
                    }
                    Ok(Completed { returncode, stdout: String::new(), stderr })
                }
                Err("timeout") => Err(RunError::TimedOut),
                Err(_) => Err(RunError::Start(io::Error::from(io::ErrorKind::NotFound))),
            }
        }

        fn detach(&self, command: &str) -> io::Result<()> {
            self.control_calls.lock().unwrap().push(format!("detached {command}"));
            Ok(())
        }
    }

    /// A scratch engine: built (a rip helper that fails, a control script that exists), not
    /// signed in, a running service signed in to one storefront.
    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        config: Arc<Config>,
        http: Arc<FakeHttp>,
        stage: Stage,
        lossless: Arc<Lossless>,
        cache: MediaCache,
    }

    impl Fixture {
        fn new() -> Fixture {
            Fixture::with_config(|_| {})
        }

        fn with_config(adjust: impl FnOnce(&mut Config)) -> Fixture {
            let (dir, config) = scratch(|config| {
                let root = config.root.clone();
                config.lossless_data_dir = root.join("data");
                config.lossless_rip_helper = root.join("amui-fairplay-rip");
                config.lossless_wrapperctl = root.join("wrapperctl-stand-in.py");
                adjust(config);
            });
            let root = config.root.clone();
            fs::create_dir(root.join("data")).unwrap();
            write_program(&config.lossless_rip_helper, "#!/bin/sh\nexit 1\n");
            fs::write(&config.lossless_wrapperctl, "").unwrap();
            let script = Script {
                regions: Some(vec![STOREFRONT.to_string()]),
                dispatch: json!({"adamId": "1000", "m3u8": MASTER_URL}),
                master: Ok(device_master().into_bytes()),
                start: Ok((0, String::new())),
                started_regions: Some(vec![STOREFRONT.to_string()]),
            };
            let stage = Stage {
                script: Arc::new(Mutex::new(script)),
                service_calls: Arc::default(),
                control_calls: Arc::default(),
            };
            let answers = stage.script.clone();
            let http = Arc::new(FakeHttp::new(move |_| answers.lock().unwrap().master.clone()));
            let lossless = Arc::new(Lossless::with_parts(
                config.clone(),
                http.clone(),
                Box::new(stage.clone()),
                Box::new(stage.clone()),
            ));
            let cache = MediaCache::in_directory(config.clone(), root.join("media"), config::MEDIA_CACHE_BYTES);
            Fixture { _dir: dir, root, config, http, stage, lossless, cache }
        }

        fn sign_in(&self) {
            for name in ACCOUNT_FILES {
                fs::write(self.config.lossless_data_dir.join(name), b"x").unwrap();
            }
        }

        fn script(&self) -> MutexGuard<'_, Script> {
            self.stage.script.lock().unwrap()
        }

        fn service_calls(&self) -> Vec<String> {
            self.stage.service_calls.lock().unwrap().clone()
        }

        fn control_calls(&self) -> Vec<String> {
            self.stage.control_calls.lock().unwrap().clone()
        }
    }

    fn unavailable_text(result: Result<impl Sized, AcquireError>) -> String {
        match result {
            Err(AcquireError::Lossless(LosslessError::Unavailable(message))) => message,
            Err(other) => panic!("another error: {other:?}"),
            Ok(_) => panic!("no error"),
        }
    }

    // --- availability

    #[test]
    fn configured_needs_helper_and_account_files() {
        let fixture = Fixture::new();
        assert!(helper_built(&fixture.config));
        assert!(!signed_in_files(&fixture.config));
        assert!(!fixture.lossless.configured());
        fixture.sign_in();
        fixture.lossless.clear_backoff(); // also drops configured()'s two-second memory
        assert!(fixture.lossless.configured());
        fs::write(fixture.config.lossless_data_dir.join("MUSIC_TOKEN"), b"").unwrap(); // empty file = not signed in
        assert!(!signed_in_files(&fixture.config));
        fixture.lossless.clear_backoff();
        assert!(!fixture.lossless.configured());
    }

    #[test]
    fn backoff_after_a_service_failure() {
        let fixture = Fixture::new();
        fixture.sign_in();
        assert!(fixture.lossless.available());
        let error = fixture.lossless.fail("boom");
        assert_eq!(error, LosslessError::Unavailable("boom".to_string()));
        assert!(!fixture.lossless.available());
        assert!(fixture.lossless.configured()); // the back-off hides availability, not configuration
        fixture.lossless.clear_backoff();
        assert!(fixture.lossless.available());
    }

    #[test]
    fn acquire_without_sign_in_is_unavailable_without_network() {
        let fixture = Fixture::new();
        let message = unavailable_text(fixture.lossless.acquire_lossless("1", &fixture.cache, None, None));
        assert!(message.contains("sign in with Connect"), "{message}");
        assert!(fixture.service_calls().is_empty(), "network used");
        assert!(fixture.http.asked.lock().unwrap().is_empty(), "network used");
    }

    #[test]
    fn ensure_service_reports_a_signed_out_service() {
        let fixture = Fixture::new();
        fixture.sign_in();
        fixture.script().regions = Some(Vec::new());
        let error = fixture.lossless.ensure_service(None).unwrap_err();
        assert!(matches!(&error, LosslessError::Unavailable(message) if message.contains("not signed in")), "{error}");
        assert!(!fixture.lossless.available()); // backing off
    }

    #[test]
    fn ensure_service_returns_regions_without_starting() {
        let fixture = Fixture::new();
        fixture.sign_in();
        assert_eq!(fixture.lossless.ensure_service(None).unwrap(), [STOREFRONT]);
        assert!(fixture.control_calls().is_empty(), "start attempted");
    }

    #[test]
    fn status_is_secret_free() {
        let fixture = Fixture::new();
        fixture.sign_in();
        let state = fixture.lossless.status();
        assert_eq!(state["storefronts"], json!([STOREFRONT]));
        assert!(state["built"] == true && state["signed_in_files"] == true && state["running"] == true);
        let keys: Vec<&str> = state.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["built", "signed_in_files", "running", "storefronts", "available", "backoff_s", "max_sample_rate", "url"]
        );
    }

    // --- what the playback dispatch answers with: a playlist, or (no lossless rendition) a
    // media file

    #[test]
    fn a_playlist_is_fetched() {
        let fixture = Fixture::new();
        assert_eq!(fixture.lossless.device_master("1000").unwrap(), (device_master(), MASTER_URL.to_string()));
        let asked = fixture.http.asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!((asked[0].url.as_str(), asked[0].auth), (MASTER_URL, false));
        assert_eq!(fixture.service_calls(), ["/m3u8?adamId=1000"]);
    }

    #[test]
    fn a_media_file_means_no_lossless_rendition_and_is_not_fetched() {
        let fixture = Fixture::new();
        fixture.script().dispatch = json!({"adamId": "1000", "m3u8": MEDIA_URL});
        let error = fixture.lossless.device_master("1000").unwrap_err();
        assert!(matches!(error, AcquireError::Lossless(LosslessError::NoLossless(_))), "{error:?}");
        assert!(fixture.http.asked.lock().unwrap().is_empty());
        assert!(!error.to_string().contains("accessKey"));
        assert!(!error.to_string().contains("itunes"));
    }

    #[test]
    fn a_playlist_name_with_other_content_means_no_lossless_rendition() {
        let fixture = Fixture::new();
        let mut media = b"\x00\x00\x00\x20ftypM4A \x00\x00\x00\x00".to_vec();
        media.extend(128..=255u8);
        fixture.script().master = Ok(media);
        assert!(matches!(fixture.http.get_text(MASTER_URL), Err(WebError::NotText(_))));
        let error = fixture.lossless.device_master("1000").unwrap_err();
        assert!(matches!(error, AcquireError::Lossless(LosslessError::NoLossless(_))), "{error:?}");
    }

    #[test]
    fn no_answer_is_a_service_failure_and_a_network_failure_stays_one() {
        let fixture = Fixture::new();
        fixture.script().dispatch = json!({"adamId": "1000", "m3u8": null});
        let error = fixture.lossless.device_master("1000").unwrap_err();
        assert!(matches!(error, AcquireError::Lossless(LosslessError::Unavailable(_))), "{error:?}");
        fixture.script().dispatch = json!({"adamId": "1000", "m3u8": MASTER_URL});
        fixture.script().master = Err(WebError::other("timed out"));
        let error = fixture.lossless.device_master("1000").unwrap_err();
        assert_eq!(error, AcquireError::Web(WebError::other("timed out")));
        assert!(!error.is_lossless());
    }

    // --- acquire_lossless with a stand-in service and a stand-in rip helper that copies a real
    // tiny ALAC file

    /// A stand-in helper: notes its arguments and the URL it reads from stdin, then copies
    /// `source` to `--out`, fails like the real one, or hangs.
    fn rip_helper(mode: &str, source: &Path, record: &Path) -> String {
        let act = match mode {
            "error" => "echo 'amui-fairplay-rip: rip: Get \"https://cdn.example/secret?sig=1\": refused' >&2\nexit 1\n"
                .to_string(),
            "hang" => "exec sleep 30\n".to_string(),
            _ => format!("cp '{}' \"$out\"\necho \"done\"\n", source.display()),
        };
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{record}'\nout=\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = --out ]; then out=$2; fi\n  \
             shift\ndone\nIFS= read -r url\nprintf '%s\\n' \"$url\" >> '{record}'\ncase \"$url\" in\n  https://*) ;;\n  \
             *) echo 'no playlist URL' >&2; exit 2 ;;\nesac\n{act}",
            record = record.display()
        )
    }

    /// The fixture signed in, with a tone file and the stand-in helper in `mode`; `None` when
    /// this machine has no ffmpeg that can make the tone.
    fn flow(mode: &str, codec: &str) -> Option<Fixture> {
        flow_at(mode, codec, 48000)
    }

    /// `flow()` with a tone file of another sample rate.
    fn flow_at(mode: &str, codec: &str, sample_rate: u32) -> Option<Fixture> {
        let fixture = Fixture::new();
        let source = fixture.root.join(format!("tone-{codec}.m4a"));
        if !tone(&source, codec, sample_rate) {
            eprintln!("skipped: ffmpeg/ffprobe needed");
            return None;
        }
        fixture.sign_in();
        write_program(
            &fixture.config.lossless_rip_helper,
            &rip_helper(mode, &source, &fixture.root.join("rip-record")),
        );
        Some(fixture)
    }

    fn left_over(fixture: &Fixture, ending: &str) -> Vec<String> {
        let mut names = files_in(&fixture.cache.directory);
        names.extend(files_in(&fixture.config.tmp_dir));
        names.into_iter().filter(|name| name.ends_with(ending)).collect()
    }

    #[test]
    fn acquire_stores_verified_alac_with_meta_and_hits_the_cache() {
        let Some(fixture) = flow("copy", "alac") else { return };
        let acquirer = Acquirer::new(fixture.config.clone(), fixture.http.clone(), fixture.lossless.clone());
        let mut phases = Vec::new();
        let mut progress = |phase: &str, _fraction: Option<f64>| phases.push(phase.to_string());
        let result = acquirer.acquire("123", &fixture.cache, "lossless", Some(&mut progress), None).unwrap();
        assert_eq!(phases, ["manifest", "download", "verify", "done"]);
        assert!(!result.from_cache);
        assert_eq!(result.variant["audio_group"], "audio-alac-stereo-48000-24");
        assert_eq!((&result.variant["sample_rate"], &result.variant["bit_depth"]), (&json!(48000), &json!(24)));
        let info = fs::metadata(&result.path).unwrap();
        assert!(info.is_file());
        assert_eq!(info.permissions().mode() & 0o777, 0o600);
        assert_eq!(result.bytes, info.len());
        assert!((result.duration_s.unwrap() - 0.5).abs() < 0.05);
        let meta = fixture.cache.meta("123.lossless").unwrap().unwrap();
        assert_eq!(meta["codec"], "alac");
        assert_eq!(meta["channels"], 2);
        assert_eq!(meta["variant"], "lossless");
        assert_eq!(meta["bit_depth"], 24);
        assert_eq!(left_over(&fixture, ".rip.log"), Vec::<String>::new()); // the helper log is removed
        let again = acquirer.acquire("123", &fixture.cache, "lossless", None, None).unwrap();
        assert!(again.from_cache);
        assert_eq!(again.variant, result.variant);
        // what the helper was started with: the service, the song, a staging file; the rendition on stdin
        let record: Vec<String> = BufReader::new(fs::File::open(fixture.root.join("rip-record")).unwrap())
            .lines()
            .map(|line| line.unwrap())
            .collect();
        assert_eq!(record[..5], ["--lite", fixture.config.lossless_url.as_str(), "--adam", "123", "--out"]);
        assert!(
            record[5].starts_with(fixture.config.tmp_dir.to_str().unwrap()) && record[5].ends_with(".decrypted.m4a")
        );
        assert_eq!(
            record[6..],
            [
                "https://aod.itunes.apple.com/itunes-assets/HLSMusic/v4/ab/cd/audio-alac-stereo-48000-24/P1000_default.m3u8"
            ]
        );
        assert_eq!(fixture.service_calls(), ["/status", "/m3u8?adamId=123"]);
        assert_eq!(fixture.config.lossless_url, "http://127.0.0.1:9");
    }

    #[test]
    fn hires_is_fetched_and_kept_apart_from_the_48_khz_file() {
        let Some(fixture) = flow_at("copy", "alac", 192000) else { return };
        let acquirer = Acquirer::new(fixture.config.clone(), fixture.http.clone(), fixture.lossless.clone());
        let result = acquirer.acquire("123", &fixture.cache, "hires", None, None).unwrap();
        assert!(!result.from_cache);
        assert!(result.path.ends_with("/123.hires.m4a"), "{}", result.path);
        assert_eq!(result.variant["audio_group"], "audio-alac-stereo-192000-24");
        assert_eq!((&result.variant["sample_rate"], &result.variant["bit_depth"]), (&json!(192000), &json!(24)));
        // the helper was given the rendition above 48 kHz
        let record = fs::read_to_string(fixture.root.join("rip-record")).unwrap();
        assert!(record.contains("/audio-alac-stereo-192000-24/P1000_default.m3u8"), "{record}");
        let meta = fixture.cache.meta("123.hires").unwrap().unwrap();
        assert_eq!((&meta["variant"], &meta["sample_rate"]), (&json!("hires"), &json!(192000)));
        // nothing was stored as the 48 kHz version
        assert_eq!(fixture.cache.get("123.lossless").unwrap(), None);
        let again = acquirer.acquire("123", &fixture.cache, "hires", None, None).unwrap();
        assert!(again.from_cache);
        assert_eq!(again.variant, result.variant);

        // the 48 kHz version is a file of its own next to it
        let plain = fixture.root.join("tone-48000.m4a");
        assert!(tone(&plain, "alac", 48000));
        let record_path = fixture.root.join("rip-record");
        write_program(&fixture.config.lossless_rip_helper, &rip_helper("copy", &plain, &record_path));
        let lossless = acquirer.acquire("123", &fixture.cache, "lossless", None, None).unwrap();
        assert!(lossless.path.ends_with("/123.lossless.m4a"), "{}", lossless.path);
        assert_eq!(lossless.variant["sample_rate"], 48000);
        assert_ne!(lossless.path, result.path);
        assert!(Path::new(&result.path).is_file() && Path::new(&lossless.path).is_file());
    }

    #[test]
    fn a_decode_of_hires_says_it_was_resampled() {
        let Some(fixture) = flow_at("copy", "alac", 96000) else { return };
        let out = fixture.root.join("pcm/hires.f32le");
        let frames = decode_stereo(&fixture.root.join("tone-alac.m4a"), &out).unwrap();
        assert!((frames as f64 / f64::from(config::SAMPLE_RATE) - 0.5).abs() < 0.05, "{frames} frames at 48 kHz");
        let meta = util::read_json_object(&out.with_file_name("meta.json")).unwrap();
        assert_eq!((&meta["source_sample_rate"], &meta["sample_rate"]), (&json!(96000), &json!(48000)));
        assert_eq!(meta["resampled"], true);
    }

    #[test]
    fn a_file_that_is_not_the_rendition_is_not_kept_as_hires() {
        // the service hands out 48 kHz audio under the name of the 192 kHz rendition
        let Some(fixture) = flow_at("copy", "alac", 48000) else { return };
        let message = unavailable_text(fixture.lossless.acquire_hires("129", &fixture.cache, None, None));
        assert!(message.contains("sample rate"), "{message}");
        assert_eq!(fixture.cache.get("129.hires").unwrap(), None);
    }

    #[test]
    fn a_song_without_hires_is_told_apart_from_a_failing_route() {
        let Some(fixture) = flow_at("copy", "alac", 96000) else { return };
        fixture.script().master = Ok(master(&[("audio-alac-stereo-48000-24", "alac", 1995449)]).into_bytes());
        let error = fixture.lossless.acquire_hires("130", &fixture.cache, None, None).unwrap_err();
        assert_eq!(error, AcquireError::Lossless(LosslessError::NoHiRes(NO_HIRES.to_string())));
        assert!(fixture.lossless.available(), "a per-song answer, not a service failure");
        assert_eq!(fixture.cache.get("130.hires").unwrap(), None);
    }

    #[test]
    fn the_route_says_why_it_cannot_be_used() {
        let fixture = Fixture::new();
        let problem = || fixture.lossless.problem().unwrap_or_default();
        assert!(problem().contains("sign-in"), "{}", problem());
        fixture.sign_in();
        fixture.lossless.clear_backoff();
        assert_eq!(fixture.lossless.problem(), None);
        fixture.lossless.fail("boom");
        assert!(problem().contains("failed a moment ago"), "{}", problem());
        fixture.lossless.clear_backoff();
        fs::remove_file(&fixture.config.lossless_rip_helper).unwrap();
        assert!(problem().contains("not built"), "{}", problem());
    }

    #[test]
    fn a_non_alac_result_is_rejected() {
        let Some(fixture) = flow("copy", "aac") else { return };
        let message = unavailable_text(fixture.lossless.acquire_lossless("124", &fixture.cache, None, None));
        assert!(message.contains("not stereo ALAC"), "{message}");
        assert_eq!(fixture.cache.get("124.lossless").unwrap(), None);
        assert_eq!(left_over(&fixture, ".decrypted.m4a"), Vec::<String>::new());
    }

    #[test]
    fn helper_failure_is_redacted_and_arms_the_backoff() {
        let Some(fixture) = flow("error", "alac") else { return };
        let message = unavailable_text(fixture.lossless.acquire_lossless("125", &fixture.cache, None, None));
        assert!(message.contains("decryption failed"), "{message}");
        assert!(!message.contains("sig=1"), "{message}");
        assert!(!message.contains("cdn.example"), "{message}");
        assert!(!fixture.lossless.available());
        let calls = fixture.service_calls().len();
        let again = unavailable_text(fixture.lossless.acquire_lossless("126", &fixture.cache, None, None));
        assert_eq!(again, "the lossless service failed a moment ago; trying again shortly");
        assert_eq!(fixture.service_calls().len(), calls); // inside the back-off: no attempt
    }

    #[test]
    fn cancel_stops_the_helper() {
        let Some(fixture) = flow("hang", "alac") else { return };
        let cancel = Arc::new(Event::new());
        let later = cancel.clone();
        let timer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            later.set();
        });
        let started = Instant::now();
        let error = fixture.lossless.acquire_lossless("127", &fixture.cache, None, Some(&*cancel)).unwrap_err();
        assert!(error.is_cancelled(), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(left_over(&fixture, ".decrypted.m4a"), Vec::<String>::new());
        timer.join().unwrap();
    }

    #[test]
    fn no_alac_rendition() {
        let Some(fixture) = flow("copy", "alac") else { return };
        fixture.script().master = Ok(master(&[("audio-stereo-256", "mp4a.40.2", 270717)]).into_bytes());
        let error = fixture.lossless.acquire_lossless("128", &fixture.cache, None, None).unwrap_err();
        assert!(matches!(error, AcquireError::Lossless(LosslessError::NoLossless(_))), "{error:?}");
        assert!(fixture.lossless.available()); // a per-song answer, not a service failure
    }

    #[test]
    fn decode_stereo_handles_alac() {
        let Some(fixture) = flow("copy", "alac") else { return };
        let out = fixture.root.join("pcm/lossless.f32le");
        let frames = decode_stereo(&fixture.root.join("tone-alac.m4a"), &out).unwrap();
        assert_eq!(frames, fs::metadata(&out).unwrap().len() / 8);
        assert!((frames as f64 / f64::from(config::SAMPLE_RATE) - 0.5).abs() < 0.05);
        let meta = util::read_json_object(&out.with_file_name("meta.json")).unwrap();
        assert_eq!(meta["source"], "alac");
        assert_eq!(meta["source_sample_rate"], 48000);
        assert_eq!(meta["resampled"], false);
    }

    // --- beyond the Python tests

    #[test]
    fn the_route_can_be_shared_between_threads() {
        fn shared<T: Send + Sync + 'static>() {}
        shared::<Lossless>();
        shared::<LoopbackService>();
    }

    #[test]
    fn the_service_is_started_when_nothing_answers() {
        let fixture = Fixture::new();
        fixture.sign_in();
        fixture.script().regions = None;
        let mut phases = Vec::new();
        let mut progress = |phase: &str, fraction: Option<f64>| phases.push((phase.to_string(), fraction));
        assert_eq!(fixture.lossless.ensure_service(Some(&mut progress)).unwrap(), [STOREFRONT]);
        assert_eq!(phases, [("service".to_string(), None)]);
        assert_eq!(fixture.control_calls(), ["start"]);
        assert_eq!(fixture.service_calls(), ["/status", "/status"]);
        // only what this engine started is stopped, once, without waiting
        fixture.lossless.stop_service_if_started();
        fixture.lossless.stop_service_if_started();
        assert_eq!(fixture.control_calls(), ["start", "detached stop"]);
        let other = Fixture::new();
        other.sign_in();
        other.lossless.ensure_service(None).unwrap();
        other.lossless.stop_service_if_started();
        assert!(other.control_calls().is_empty());
    }

    #[test]
    fn a_service_that_does_not_start_arms_the_backoff() {
        let attempt = |start: Result<(i32, String), &'static str>, started: Option<Vec<String>>, signed_in: bool| {
            let fixture = Fixture::new();
            if signed_in {
                fixture.sign_in();
            }
            {
                let mut script = fixture.script();
                script.regions = None;
                script.start = start;
                script.started_regions = started;
            }
            let error = fixture.lossless.ensure_service(None).unwrap_err().to_string();
            (error, fixture.lossless.available(), fixture.control_calls().len())
        };
        let trouble = "Traceback\n  wrapperctl: no answer from https://service.example/start?token=1 (timeout)\n\n";
        assert_eq!(
            attempt(Ok((1, trouble.to_string())), None, true),
            (
                "could not start the lossless service:   wrapperctl: no answer from <url> (timeout)".to_string(),
                false,
                1
            )
        );
        assert_eq!(attempt(Ok((3, String::new())), None, true).0, "could not start the lossless service: exit 3");
        assert_eq!(attempt(Err("timeout"), None, true).0, "the lossless helper 'start' timed out");
        assert_eq!(attempt(Err("missing"), None, true).0, "could not run the lossless helper (FileNotFoundError)");
        assert_eq!(
            attempt(Ok((0, String::new())), Some(Vec::new()), true),
            ("the lossless service started but reports no signed-in account".to_string(), false, 1)
        );
        // without the account files nothing is started, and that is no service failure
        let (error, _, starts) = attempt(Ok((0, String::new())), None, false);
        assert_eq!((error.as_str(), starts), ("lossless needs Apple's native sign-in: sign in with Connect", 0));
        // not built at all
        let fixture = Fixture::new();
        fs::remove_file(&fixture.config.lossless_rip_helper).unwrap();
        let error = fixture.lossless.ensure_service(None).unwrap_err().to_string();
        assert_eq!(error, "the lossless route is not built (tools/lossless/build.sh)");
        let message = unavailable_text(fixture.lossless.acquire_lossless("1", &fixture.cache, None, None));
        assert_eq!(message, "the lossless route is not built (./setup.sh builds it)");
        let state = fixture.lossless.status();
        assert!(state["built"] == false && state["running"] == false && state["storefronts"] == json!([]));
        assert!(fixture.service_calls().is_empty());
    }

    /// The route as the engine builds it, on a machine where nothing of it is built: every
    /// answer comes from the file system, and neither the service nor the control script is
    /// asked.
    #[test]
    fn the_real_route_asks_nothing_on_an_unbuilt_machine() {
        let (_dir, config) = scratch(|_| {});
        let http = Arc::new(FakeHttp::unused());
        let lossless = Lossless::new(config.clone(), http.clone());
        assert!(!lossless.configured() && !lossless.available());
        let state = lossless.status();
        assert!(state["built"] == false && state["signed_in_files"] == false && state["running"] == false);
        assert_eq!((&state["url"], &state["max_sample_rate"]), (&json!("http://127.0.0.1:9"), &json!(48000)));
        assert_eq!(state["backoff_s"], 0.0);
        let cache = MediaCache::in_directory(config.clone(), config.tmp_dir.join("media"), config::MEDIA_CACHE_BYTES);
        let message = unavailable_text(lossless.acquire_lossless("1", &cache, None, None));
        assert_eq!(message, "the lossless route is not built (./setup.sh builds it)");
        let error = lossless.ensure_service(None).unwrap_err().to_string();
        assert_eq!(error, "the lossless route is not built (tools/lossless/build.sh)");
        lossless.stop_service_if_started(); // it started nothing
        assert!(http.asked.lock().unwrap().is_empty());
    }

    /// A service that answers every request with one body.
    struct Canned(Result<Vec<u8>, String>);

    impl Service for Canned {
        fn get(&self, _path: &str, _timeout: f64) -> Result<Vec<u8>, String> {
            self.0.clone()
        }
    }

    #[test]
    fn service_answers_are_unwrapped() {
        let ask = |answer: Result<&str, &str>| {
            let fixture = Fixture::new();
            let canned = Canned(answer.map(|body| body.as_bytes().to_vec()).map_err(str::to_owned));
            let lossless = Lossless::with_parts(
                fixture.config.clone(),
                fixture.http.clone(),
                Box::new(canned),
                Box::new(fixture.stage.clone()),
            );
            (lossless.get_json("/status", 2.0).map_err(|error| error.to_string()), lossless.service_status(2.0))
        };
        let (data, regions) = ask(Ok("{\"code\": 0, \"data\": {\"regions\": [\"100000\", 100001]}}"));
        assert_eq!(data.unwrap()["regions"], json!(["100000", 100001]));
        assert_eq!(regions, Some(vec!["100000".to_string(), "100001".to_string()]));
        assert_eq!(ask(Ok("{\"code\": 0}")), (Ok(Map::new()), Some(Vec::new())));
        assert_eq!(ask(Ok("{\"code\": 0, \"data\": {\"regions\": \"x\"}}")).1, Some(Vec::new()));
        let refused = "{\"code\": -1, \"msg\": \"no dispatch for https://service.example/a?sig=1 today\"}";
        let expected = "the lossless service refused: no dispatch for <url> today";
        assert_eq!(ask(Ok(refused)), (Err(expected.to_string()), None));
        assert_eq!(ask(Ok("{\"data\": {}}")).0, Err("the lossless service refused: error".to_string()));
        let long = format!("{{\"code\": 1, \"msg\": \"{}\"}}", "x".repeat(300));
        assert_eq!(ask(Ok(&long)).0.unwrap_err().len(), "the lossless service refused: ".len() + 120);
        assert_eq!(ask(Ok("[1]")).0, Err("the lossless service returned an unexpected answer".to_string()));
        assert_eq!(ask(Ok("<html>")).0, Err("the lossless service does not answer (JSONDecodeError)".to_string()));
        let silent = ask(Err("ConnectionRefusedError"));
        assert_eq!(silent, (Err("the lossless service does not answer (ConnectionRefusedError)".to_string()), None));
    }

    /// Answer one HTTP request on `listener` with `status` and `body`; the request's first line
    /// comes back.
    fn serve_once(
        listener: std::net::TcpListener,
        status: &'static str,
        body: &'static str,
    ) -> thread::JoinHandle<String> {
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            request.read_line(&mut first).unwrap();
            let mut line = String::new();
            while request.read_line(&mut line).unwrap() > 2 {
                line.clear();
            }
            let answer = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(answer.as_bytes()).unwrap();
            first.trim_end().to_string()
        })
    }

    /// A loopback port that is taken (by the socket that comes back) but not listened on: a
    /// connection to it is refused, and no other program can start listening there meanwhile.
    fn refusing_port() -> (std::os::fd::OwnedFd, u16) {
        use std::os::fd::FromRawFd;
        let size = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        // SAFETY: plain socket calls on a descriptor this function has just opened and an address
        // structure it owns; the descriptor is handed to an OwnedFd, which closes it.
        unsafe {
            let socket = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            assert!(socket >= 0);
            let socket = std::os::fd::OwnedFd::from_raw_fd(socket);
            let mut address: libc::sockaddr_in = std::mem::zeroed();
            address.sin_family = libc::AF_INET as libc::sa_family_t;
            address.sin_addr.s_addr = u32::from(std::net::Ipv4Addr::LOCALHOST).to_be();
            let raw = std::os::fd::AsRawFd::as_raw_fd(&socket);
            assert_eq!(libc::bind(raw, (&raw const address).cast(), size), 0);
            let mut length = size;
            assert_eq!(libc::getsockname(raw, (&raw mut address).cast(), &mut length), 0);
            (socket, u16::from_be(address.sin_port))
        }
    }

    #[test]
    fn the_real_service_is_plain_http_on_this_machine() {
        // a listener of this test on a port the system picks: never the port of a real service
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_ne!(port, 12351);
        let service = LoopbackService::new(&format!("http://127.0.0.1:{port}"));
        let server = serve_once(listener, "200 OK", "{\"code\": 0, \"data\": {\"regions\": []}}");
        let body = service.get("/status", 5.0).unwrap();
        assert_eq!(body, b"{\"code\": 0, \"data\": {\"regions\": []}}");
        assert_eq!(server.join().unwrap(), "GET /status HTTP/1.1");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let other = LoopbackService::new(&format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port()));
        let server = serve_once(listener, "500 Internal Server Error", "{}");
        assert_eq!(other.get("/m3u8?adamId=1", 5.0), Err("HTTP 500".to_string()));
        assert_eq!(server.join().unwrap(), "GET /m3u8?adamId=1 HTTP/1.1");
        // a port nothing listens on
        let (_taken, port) = refusing_port();
        let absent = LoopbackService::new(&format!("http://127.0.0.1:{port}"));
        assert_eq!(absent.get("/status", 5.0), Err("ConnectionRefusedError".to_string()));
        // a service that takes the connection and then says nothing
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mute = LoopbackService::new(&format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port()));
        let started = Instant::now();
        assert_eq!(mute.get("/status", 0.3), Err("TimeoutError".to_string()));
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(listener);
        // any other address is refused before a connection is made
        for elsewhere in ["http://192.0.2.1:80", "https://127.0.0.1:9", "http://localhost.example:9", "nonsense"] {
            assert_eq!(LoopbackService::new(elsewhere).get("/status", 0.2), Err("not a loopback address".to_string()));
        }
    }

    #[test]
    fn the_control_script_runs_under_python3_in_isolated_mode() {
        let fixture = Fixture::new();
        // the configuration names no program for the helper: the script runs under python3
        let script = fixture.config.lossless_wrapperctl.clone();
        let marker = fixture.root.join("stopped");
        let text = format!(
            "import sys, time\ncommand = sys.argv[1]\nprint(command, sys.flags.isolated)\n\
             if command == 'fail':\n    sys.stderr.write('no such command\\n')\n    sys.exit(3)\n\
             if command == 'hang':\n    time.sleep(30)\n\
             if command == 'stop':\n    open({:?}, 'w').close()\n",
            marker.to_str().unwrap()
        );
        fs::write(&script, text).unwrap();
        let control = HelperWrapperctl { config: fixture.config.clone() };
        let done = control.run("status", 30.0).unwrap();
        assert_eq!((done.returncode, done.stdout.as_str(), done.stderr.as_str()), (0, "status 1\n", ""));
        let done = control.run("fail", 30.0).unwrap();
        assert_eq!((done.returncode, done.stderr.as_str()), (3, "no such command\n"));
        let started = Instant::now();
        assert!(matches!(control.run("hang", 0.3), Err(RunError::TimedOut)));
        assert!(started.elapsed() < Duration::from_secs(10));
        control.detach("stop").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(marker.exists());
    }

    #[test]
    fn rip_reports_what_the_helper_says() {
        let fixture = Fixture::new();
        let (out, log) = (fixture.root.join("out.m4a"), fixture.root.join("rip.log"));
        let rip = |script: &str| {
            write_program(&fixture.config.lossless_rip_helper, script);
            fixture.lossless.clear_backoff();
            fixture.sign_in();
            let url = "https://aod.itunes.apple.com/a/b.m3u8";
            let result = fixture.lossless.rip("1", url, &out, &log, None, RIP_TIMEOUT_S);
            (result.map_err(|error| error.to_string()), fixture.lossless.available())
        };
        // a refusal of the helper is a local fault: no back-off
        let refusal = (Err("lossless helper refused: amui-fairplay-rip: output file already exists".to_string()), true);
        assert_eq!(rip("#!/bin/sh\necho 'amui-fairplay-rip: output file already exists' >&2\nexit 1\n"), refusal);
        assert_eq!(rip("#!/bin/sh\nexit 4\n"), (Err("lossless decryption failed: exit status 4".to_string()), false));
        let silent = (Err("the lossless helper reported success but wrote no output".to_string()), false);
        assert_eq!(rip("#!/bin/sh\nexit 0\n"), silent);
        // what the helper prints goes to the private log; its file is private as well
        let works = "#!/bin/sh\nread -r url\necho \"fetching\"\nprintf 'media' > \"$6\"\n";
        assert_eq!(rip(works), (Ok(5), true));
        assert_eq!(fs::read_to_string(&log).unwrap(), "fetching\n");
        for file in [&out, &log] {
            assert_eq!(fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o600, "{file:?}");
        }
        // a helper that does not come to an end is killed at the time limit
        write_program(&fixture.config.lossless_rip_helper, "#!/bin/sh\nexec sleep 30\n");
        let started = Instant::now();
        let slow = fixture.lossless.rip("1", "https://aod.itunes.apple.com/a/b.m3u8", &out, &log, None, 0.2);
        assert_eq!(slow.unwrap_err().to_string(), "the lossless download timed out");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(!fixture.lossless.available());
        fs::remove_file(&fixture.config.lossless_rip_helper).unwrap();
        let missing = fixture.lossless.rip("1", "https://aod.itunes.apple.com/a/b.m3u8", &out, &log, None, 1.0);
        assert_eq!(missing.unwrap_err().to_string(), "the lossless route is not built (tools/lossless/build.sh)");
    }

    #[test]
    fn renditions_and_labels_as_python_reads_them() {
        // a rendition two lines below its tag, a tag without a URL, an unknown group, quoted numbers
        let text = "#EXTM3U\r\n#EXT-X-STREAM-INF:BANDWIDTH=\"5\",CODECS=\"alac\",AUDIO=\"audio-alac-stereo-48000-24\"\r\n\
                    #EXT-X-NOTE\r\na/b.m3u8\r\n\
                    #EXT-X-STREAM-INF:BANDWIDTH=9,CODECS=\"alac\",AUDIO=\"audio-alac-stereo-96000-24\"\r\n#A\r\n#B\r\nc.m3u8\r\n\
                    #EXT-X-STREAM-INF:BANDWIDTH=9,CODECS=\"alac\",AUDIO=\"audio-alac-surround-48000-24\"\r\nd.m3u8\r\n\
                    #EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=7,BANDWIDTH=9,CODECS=\"alac\",AUDIO=\"audio-alac-stereo-48000-16\"\r\ne.m3u8\r\n";
        let variants = alac_variants(text, MASTER_URL);
        let seen: Vec<(&str, &str, u32)> =
            variants.iter().map(|v| (v.audio_group.as_str(), v.bandwidth.as_str(), v.bit_depth)).collect();
        assert_eq!(seen, [("audio-alac-stereo-48000-24", "5", 24), ("audio-alac-stereo-48000-16", "7", 16)]);
        assert!(variants[0].url.ends_with("/ab/cd/a/b.m3u8"));
        // ties: the deeper one, then the first
        assert_eq!(select_alac(text, MASTER_URL, 48000).unwrap().bit_depth, 24);
        let shown = format!("{:?}", variants[0]);
        assert!(!shown.contains("https://") && shown.contains("audio-alac-stereo-48000-24"), "{shown}");
        for (value, text) in [
            (44.1, "44.1"),
            (48.0, "48"),
            (88.2, "88.2"),
            (176.4, "176.4"),
            (352.8, "352.8"),
            (11.025, "11.025"),
            (8.0, "8"),
            (0.001, "0.001"),
            (1234.567, "1234.57"),
            (1234567.0, "1.23457e+06"),
            (0.0441, "0.0441"),
            (1.234e-05, "1.234e-05"),
            (999999.5, "1e+06"),
            (99999.95, "99999.9"),
            (0.5, "0.5"),
            (0.0, "0"),
        ] {
            assert_eq!(general(value), text);
        }
        assert_eq!(quality_label(Some(88200), None), "Hi-Res Lossless · ALAC 88.2 kHz");
        assert_eq!(quality_label(Some(0), Some(16)), "Lossless · ALAC 16-bit");
        assert_eq!(
            redact("Get \"https://cdn.example/a?sig=1\": refused by http://h/x"),
            "Get \"<url> refused by <url>"
        );
    }
}
