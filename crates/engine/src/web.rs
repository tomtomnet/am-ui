//! HTTP layer for Apple Music web endpoints, with the workspace's security rules.
//!
//! * HTTPS only, default certificate verification, no redirects with credentials.
//! * Account credentials (the account's music user token plus Apple's public web developer
//!   token) are sent to exactly two hosts: amp-api.music.apple.com and play.itunes.apple.com
//!   (license / web playback). CDN, artwork and manifest requests carry no credentials.
//! * The music user token comes from one of two sign-ins ([`Web::token_sources`]): the native
//!   one ("android": the file Apple's Android libraries keep in the private lossless runtime,
//!   which also unlocks ALAC) or the browser session ("browser": the workspace's saved cookie).
//!   The native one is used first; when Apple rejects one and accepts the other, the accepted
//!   one is used from then on. Both files are read at request time, must be private, and their
//!   contents go nowhere but the Cookie header of those two hosts.
//! * Only an allowlist of Apple hosts is ever contacted ([`validate_url`]).
//! * Token values, signed URLs and response bodies are never logged or put in errors. Errors
//!   carry the host, the HTTP status and, for a refusal, Apple's own short title of it.
//!
//! Two things differ from the Python layer. A 403 is a rejected sign-in only when Apple's error
//! code says so (40300, 40301) or says nothing: code 40304 ("Not Entitled", Apple's answer for
//! an item the account may not have, such as a song whose catalog version was withdrawn) becomes
//! [`WebError::NotEntitled`], and no sign-in is given up for it. And a 401, Apple's answer to a
//! developer token it no longer accepts, fetches the current developer token and asks again once.
//! A 401 or 403 to a request that carried no credentials (a manifest link that expired, a page
//! that is not public) is a plain error, not a rejected sign-in.
//!
//! Everything is synchronous; callers run it on worker threads. One [`Web`] is shared by the
//! whole engine (it keeps connections open between requests). Code that only needs to make
//! requests takes a [`Http`], which tests replace.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use regex::Regex;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::config::{self, Config};
use crate::util::{self, Event};

pub const USER_AGENT: &str = "Mozilla/5.0";
pub const NO_SIGN_IN: &str = "No Apple Music sign-in on this machine: connect to sign in";
const TOKEN_MAX_BYTES: usize = 32768;
/// Refresh the developer token when less than a day of validity remains.
const DEV_MIN_REMAINING: f64 = 24.0 * 3600.0;
const BROWSE_URL: &str = "https://music.apple.com/us/browse";

/// Network/HTTP failure with a message that is safe to show and log.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum WebError {
    /// No sign-in on this machine, or Apple rejected it (401/403): reconnect needed. `status` is
    /// the HTTP status when Apple answered.
    #[error("{message}")]
    Auth { message: String, status: Option<u16> },
    #[error("{0}")]
    NotFound(String),
    #[error("{message}")]
    RateLimited { message: String, retry_after: Option<f64> },
    /// Apple refuses this one item to the account (HTTP 403 with its code 40304, "Not Entitled"):
    /// a song whose catalog version was withdrawn, for example. The sign-in is fine.
    #[error("{0}")]
    NotEntitled(String),
    /// A response fetched as a text playlist is not text (a media file, for one).
    #[error("{0}")]
    NotText(String),
    #[error("{0}")]
    Cancelled(String),
    #[error("{0}")]
    Other(String),
}

impl WebError {
    pub fn other(message: impl Into<String>) -> WebError {
        WebError::Other(message.into())
    }

    pub fn auth(message: impl Into<String>, status: Option<u16>) -> WebError {
        WebError::Auth { message: message.into(), status }
    }

    pub fn rate_limited(host: &str, retry_after: Option<f64>) -> WebError {
        let wait = match retry_after {
            Some(seconds) if seconds != 0.0 => format!("; retry after {seconds:.0}s"),
            _ => String::new(),
        };
        WebError::RateLimited { message: format!("{host} rate-limited this session{wait}"), retry_after }
    }

    pub fn is_auth(&self) -> bool {
        matches!(self, WebError::Auth { .. })
    }

    /// The HTTP status of an Apple rejection.
    pub fn status(&self) -> Option<u16> {
        match self {
            WebError::Auth { status, .. } => *status,
            _ => None,
        }
    }
}

/// One of the two sign-ins a machine can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// The native sign-in of the lossless route.
    Android,
    /// The saved browser session.
    Browser,
}

impl Source {
    /// Default order: the native sign-in first (it also unlocks lossless).
    pub const ALL: [Source; 2] = [Source::Android, Source::Browser];

    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Android => "android",
            Source::Browser => "browser",
        }
    }

    pub fn parse(text: &str) -> Option<Source> {
        match text {
            "android" => Some(Source::Android),
            "browser" => Some(Source::Browser),
            _ => None,
        }
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// (host, the URL as it is sent) for an allowed HTTPS URL. The URL is sent in the parser's own
/// spelling, so the check and the request cannot disagree about the host.
fn checked_url(url: &str, authenticated: bool) -> Result<(String, String), WebError> {
    let refused = || WebError::other("Refused an unexpected URL; no token was sent");
    let parsed = url::Url::parse(url).map_err(|_| refused())?;
    let host = parsed.host_str().unwrap_or("").to_string();
    let allowed = host == "music.apple.com"
        || host == config::API_HOST
        || host.ends_with(".itunes.apple.com")
        || host.ends_with(".mzstatic.com");
    if parsed.scheme() != "https"
        || !matches!(parsed.port(), None | Some(443))
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || !allowed
    {
        return Err(refused());
    }
    if authenticated && host != config::API_HOST && host != config::LICENSE_HOST {
        return Err(WebError::other("Account credentials may only be sent to Apple's API and license hosts"));
    }
    Ok((host, parsed.into()))
}

/// The host of `url`; an error for anything outside the allowlist or not HTTPS, and for
/// `authenticated` requests to any host but Apple's API and license hosts.
pub fn validate_url(url: &str, authenticated: bool) -> Result<String, WebError> {
    checked_url(url, authenticated).map(|(host, _)| host)
}

/// A music user token as Apple's cookie carries it: one printable value without separators.
pub fn validate_user_token(token: &str) -> Result<&str, &'static str> {
    let plain = token.chars().all(|c| (33..=126).contains(&(c as u32)) && !matches!(c, ';' | '"' | '\\'));
    if !(20..=TOKEN_MAX_BYTES).contains(&token.len()) || !plain {
        return Err("Expected one cookie value, without cookie name, quotes, spaces or other cookies");
    }
    if token.starts_with("media-user-token=") {
        return Err("Paste the cookie value only, without media-user-token=");
    }
    Ok(token)
}

fn jwt_claims(token: &str) -> Option<Map<String, Value>> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    match serde_json::from_slice(&bytes).ok()? {
        Value::Object(claims) => Some(claims),
        _ => None,
    }
}

fn jwt_expiry(token: &str) -> f64 {
    jwt_claims(token).and_then(|claims| claims.get("exp").and_then(Value::as_f64)).unwrap_or(0.0)
}

// --- one HTTP exchange -------------------------------------------------------

/// What goes on the wire for one request; redirects are never followed at this level.
pub struct Exchange<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub headers: &'a [(&'a str, String)],
    pub body: Option<&'a [u8]>,
    /// Limit for connecting and for every single read or write.
    pub timeout: Duration,
}

impl Exchange<'_> {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }
}

/// Status and the few response headers the engine looks at.
#[derive(Debug, Clone, Default)]
pub struct Head {
    pub status: u16,
    pub location: Option<String>,
    pub retry_after: Option<String>,
    pub content_length: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NetError {
    Timeout,
    /// A short reason without addresses or URLs ("host not found", "connection failed").
    Failed(String),
}

/// The network. [`Web`] applies every rule above this; tests answer exchanges themselves.
pub trait Transport: Send + Sync {
    fn open(&self, exchange: &Exchange) -> Result<(Head, Box<dyn Read>), NetError>;
}

/// The real network: one connection pool for all of Apple's hosts.
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl Default for UreqTransport {
    fn default() -> Self {
        UreqTransport::new()
    }
}

impl UreqTransport {
    pub fn new() -> UreqTransport {
        let agent = ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .user_agent(USER_AGENT)
            .build()
            .into();
        UreqTransport { agent }
    }
}

fn net_error(error: ureq::Error) -> NetError {
    match error {
        ureq::Error::Timeout(_) => NetError::Timeout,
        ureq::Error::HostNotFound => NetError::Failed("host not found".into()),
        ureq::Error::ConnectionFailed => NetError::Failed("connection failed".into()),
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => NetError::Failed("TLS error".into()),
        ureq::Error::Io(error) if error.kind() == std::io::ErrorKind::TimedOut => NetError::Timeout,
        ureq::Error::Io(error) => NetError::Failed(format!("{:?}", error.kind())),
        _ => NetError::Failed("request failed".into()),
    }
}

impl Transport for UreqTransport {
    fn open(&self, exchange: &Exchange) -> Result<(Head, Box<dyn Read>), NetError> {
        let limit = Some(exchange.timeout);
        macro_rules! prepared {
            ($builder:expr) => {{
                let mut builder = $builder;
                for (name, value) in exchange.headers {
                    builder = builder.header(*name, value.as_str());
                }
                builder
                    .config()
                    .timeout_resolve(limit)
                    .timeout_connect(limit)
                    .timeout_send_request(limit)
                    .timeout_recv_response(limit)
                    .timeout_per_read(limit)
                    .timeout_per_write(limit)
                    .build()
            }};
        }
        macro_rules! with_body {
            ($builder:expr) => {
                match exchange.body {
                    Some(body) => prepared!($builder).send(body),
                    None => prepared!($builder).send_empty(),
                }
            };
        }
        let response = match exchange.method {
            "GET" => prepared!(self.agent.get(exchange.url)).call(),
            "DELETE" => match exchange.body {
                Some(body) => prepared!(self.agent.delete(exchange.url)).force_send_body().send(body),
                None => prepared!(self.agent.delete(exchange.url)).call(),
            },
            "POST" => with_body!(self.agent.post(exchange.url)),
            "PUT" => with_body!(self.agent.put(exchange.url)),
            "PATCH" => with_body!(self.agent.patch(exchange.url)),
            _ => return Err(NetError::Failed("unsupported method".into())),
        }
        .map_err(net_error)?;
        let text = |name: &str| response.headers().get(name).and_then(|value| value.to_str().ok()).map(str::to_owned);
        let head = Head {
            status: response.status().as_u16(),
            location: text("location"),
            retry_after: text("retry-after"),
            content_length: text("content-length").and_then(|value| value.parse().ok()),
        };
        Ok((head, Box::new(response.into_body().into_reader())))
    }
}

// --- requests under the rules --------------------------------------------------

/// One request as the engine asks for it.
#[derive(Debug, Clone)]
pub struct Call<'a> {
    pub method: &'a str,
    pub url: &'a str,
    /// Send the account's credentials (Apple's API and license hosts only).
    pub auth: bool,
    pub body: Option<&'a [u8]>,
    pub content_type: Option<&'a str>,
    pub accept: &'a str,
    /// Seconds.
    pub timeout: f64,
    pub max_bytes: usize,
    /// Pin the sign-in whose credentials are sent (no second attempt with the other one).
    pub source: Option<Source>,
}

impl<'a> Call<'a> {
    pub fn new(method: &'a str, url: &'a str, auth: bool) -> Call<'a> {
        Call {
            method,
            url,
            auth,
            body: None,
            content_type: None,
            accept: "application/json",
            timeout: 20.0,
            max_bytes: 20_000_000,
            source: None,
        }
    }
}

/// Apple's codes for a 403 that means "this sign-in is not accepted": 40300 (Forbidden: library
/// requests without a valid music user token) and 40301 (Invalid Music User Token).
const SESSION_REFUSALS: [&str; 2] = ["40300", "40301"];
/// Apple's code for a 403 that means "this account may not have this item".
const NOT_ENTITLED: &str = "40304";

/// Apple's own code and title of a refusal (`{"errors": [{"code": "40304", "title": "Not Entitled"}]}`),
/// read from the first 64 KiB of an error answer. The title is cut down to plain words.
fn apple_refusal(body: &mut dyn Read) -> Option<(String, String)> {
    let mut data = Vec::new();
    body.take(64 * 1024).read_to_end(&mut data).ok()?;
    let parsed: Value = serde_json::from_slice(&data).ok()?;
    let error = parsed.get("errors")?.as_array()?.first()?;
    let code = error.get("code")?.as_str()?.to_string();
    let title: String = error
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ')
        .take(60)
        .collect();
    Some((code, title))
}

fn json_object(data: &[u8]) -> Result<Map<String, Value>, WebError> {
    match serde_json::from_slice(data) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(WebError::other("Apple returned an unexpected JSON shape")),
        Err(_) => Err(WebError::other("Apple returned a response that is not JSON")),
    }
}

/// Called per megabyte of a download with (bytes so far, total when the server said it).
pub type Progress<'a> = &'a mut dyn FnMut(u64, Option<u64>);

/// What the engine's modules need from the HTTP layer. [`Web`] is the real one.
pub trait Http: Send + Sync {
    /// One request. With `auth`, the credentials are those of the sign-in in use; when Apple
    /// answers 401/403 and the other sign-in exists, the request is repeated once with that one,
    /// and it becomes the sign-in in use if Apple accepts it.
    fn request(&self, call: &Call) -> Result<Vec<u8>, WebError>;

    /// Anonymous streaming download to a new private file. Returns (size, SHA-256 in hex). A set
    /// `cancel` aborts with [`WebError::Cancelled`] and removes the partial file.
    fn download(
        &self,
        url: &str,
        destination: &Path,
        max_bytes: u64,
        progress: Option<Progress>,
        cancel: Option<&Event>,
        timeout: f64,
    ) -> Result<(u64, String), WebError>;

    /// The sign-ins present on this machine, in the order requests use them.
    fn token_sources(&self) -> Vec<Source>;

    /// The sign-in requests use first, or `None` when this machine has none.
    fn token_source(&self) -> Option<Source> {
        self.token_sources().first().copied()
    }

    /// GET a JSON object with the account's credentials.
    fn get_json(&self, url: &str, params: &[(&str, &str)]) -> Result<Map<String, Value>, WebError> {
        self.get_json_with(url, params, true, 20.0, None)
    }

    /// GET a JSON object. `source` pins the sign-in whose credentials are sent.
    fn get_json_with(
        &self,
        url: &str,
        params: &[(&str, &str)],
        auth: bool,
        timeout: f64,
        source: Option<Source>,
    ) -> Result<Map<String, Value>, WebError> {
        let mut url = url.to_string();
        if !params.is_empty() {
            url.push(if url.contains('?') { '&' } else { '?' });
            url.push_str(&util::urlencode(params));
        }
        let call = Call { timeout, source, ..Call::new("GET", &url, auth) };
        json_object(&self.request(&call)?)
    }

    /// POST a JSON body with the account's credentials; the answer is a JSON object.
    fn post_json(&self, url: &str, body: &Value) -> Result<Map<String, Value>, WebError> {
        self.post_json_with(url, body, true, 30.0)
    }

    fn post_json_with(
        &self,
        url: &str,
        body: &Value,
        auth: bool,
        timeout: f64,
    ) -> Result<Map<String, Value>, WebError> {
        let data = body.to_string();
        let call = Call {
            body: Some(data.as_bytes()),
            content_type: Some("application/json"),
            timeout,
            ..Call::new("POST", url, auth)
        };
        json_object(&self.request(&call)?)
    }

    /// Anonymous text fetch (HLS manifests). Never sends credentials. [`WebError::NotText`] for
    /// anything that is not UTF-8.
    fn get_text(&self, url: &str) -> Result<String, WebError> {
        self.get_text_with(url, 20.0, 20_000_000)
    }

    fn get_text_with(&self, url: &str, timeout: f64, max_bytes: usize) -> Result<String, WebError> {
        let call = Call { accept: "*/*", timeout, max_bytes, ..Call::new("GET", url, false) };
        String::from_utf8(self.request(&call)?).map_err(|_| WebError::NotText("Manifest was not UTF-8 text".into()))
    }

    /// Anonymous binary fetch (artwork). Never sends credentials.
    fn get_bytes(&self, url: &str) -> Result<Vec<u8>, WebError> {
        self.get_bytes_with(url, 32 * 1024 * 1024, 20.0)
    }

    fn get_bytes_with(&self, url: &str, max_bytes: usize, timeout: f64) -> Result<Vec<u8>, WebError> {
        self.request(&Call { accept: "*/*", timeout, max_bytes, ..Call::new("GET", url, false) })
    }
}

#[derive(Default)]
struct DeveloperToken {
    token: Option<String>,
    expires: f64,
}

/// The engine's connection to Apple: tokens, the request rules, the network.
pub struct Web {
    config: Arc<Config>,
    transport: Box<dyn Transport>,
    developer: Mutex<DeveloperToken>,
    /// Sign-ins Apple rejected while it accepted the other one (memory only).
    demoted: Mutex<HashSet<Source>>,
}

impl Web {
    pub fn new(config: Arc<Config>) -> Web {
        Web::with_transport(config, Box::new(UreqTransport::new()))
    }

    pub fn with_transport(config: Arc<Config>, transport: Box<dyn Transport>) -> Web {
        Web { config, transport, developer: Mutex::default(), demoted: Mutex::default() }
    }

    pub fn config(&self) -> &Arc<Config> {
        &self.config
    }

    // --- Apple's public developer token ---------------------------------------

    fn developer_token_file(&self) -> PathBuf {
        self.config.state_dir.join("developer-token.json")
    }

    /// Locating the public token means reading music.apple.com's scripts; it is kept on disk.
    fn saved_developer_token(&self) -> Option<(String, f64)> {
        let saved = util::read_json_object(&self.developer_token_file())?;
        let token = saved.get("token")?.as_str()?;
        let expires = jwt_expiry(token);
        (token.matches('.').count() == 2 && expires - util::unix_now() > DEV_MIN_REMAINING)
            .then(|| (token.to_string(), expires))
    }

    fn save_developer_token(&self, token: &str) {
        // a cache miss next time is the only consequence of a failure here
        let _ = self.config.ensure_dirs();
        let saved = json!({"token": token, "expires": jwt_expiry(token), "saved_at": util::unix_now()});
        let _ = util::write_private_atomic(&self.developer_token_file(), saved.to_string().as_bytes());
    }

    fn fetch_public(&self, url: &str) -> Result<String, WebError> {
        let data = self.request(&Call { accept: "*/*", ..Call::new("GET", url, false) })?;
        Ok(String::from_utf8_lossy(&data).into_owned())
    }

    /// Find the token Apple's web player ships in its scripts. No account request is made.
    fn locate_developer_token(&self) -> Result<String, WebError> {
        static SCRIPT: OnceLock<Regex> = OnceLock::new();
        static JWT: OnceLock<Regex> = OnceLock::new();
        let script = SCRIPT.get_or_init(|| Regex::new(r#"<script[^>]+src=["']([^"']+)"#).unwrap());
        let jwt = JWT.get_or_init(|| Regex::new(r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+").unwrap());
        let html = self.fetch_public(BROWSE_URL)?;
        let mut scripts: Vec<String> = Vec::new();
        for source in script
            .captures_iter(&html)
            .map(|found| found[1].to_string())
            .filter(|src| src.contains("/assets/index"))
            .take(4)
        {
            if let Some(url) = util::urljoin("https://music.apple.com/", &source)
                && !scripts.contains(&url)
            {
                scripts.push(url);
            }
        }
        let now = util::unix_now();
        for url in scripts {
            let javascript = self.fetch_public(&url)?;
            for found in jwt.find_iter(&javascript) {
                let Some(claims) = jwt_claims(found.as_str()) else { continue };
                let current = claims.get("exp").and_then(Value::as_f64).unwrap_or(0.0) > now;
                if claims.get("iss").and_then(Value::as_str) == Some("AMPWebPlay") && current {
                    return Ok(found.as_str().to_string());
                }
            }
        }
        Err(WebError::other("Could not locate a current public web developer token; no account request was made"))
    }

    /// Apple's public web-player developer JWT (not a secret), cached in memory and on disk.
    pub fn developer_token(&self, force: bool) -> Result<String, WebError> {
        let mut developer = self.developer.lock().unwrap();
        if !force {
            if developer.token.is_none()
                && let Some((token, expires)) = self.saved_developer_token()
            {
                *developer = DeveloperToken { token: Some(token), expires };
            }
            if let Some(token) = &developer.token
                && developer.expires - util::unix_now() > DEV_MIN_REMAINING
            {
                return Ok(token.clone());
            }
        }
        let token = self.locate_developer_token()?;
        *developer = DeveloperToken { token: Some(token.clone()), expires: jwt_expiry(&token) };
        self.save_developer_token(&token);
        Ok(token)
    }

    /// Use this developer token without asking Apple's site for one (tests, tools).
    pub fn preset_developer_token(&self, token: &str, expires: f64) {
        *self.developer.lock().unwrap() = DeveloperToken { token: Some(token.to_string()), expires };
    }

    /// Fetch the developer token in the background (call once at start-up).
    pub fn warm_up(self: &Arc<Self>) {
        let web = self.clone();
        let _ = std::thread::Builder::new().name("amui-dev-token".into()).spawn(move || {
            let _ = web.developer_token(false);
        });
    }

    // --- the account's music user token ---------------------------------------

    fn token_file(&self, source: Source) -> PathBuf {
        match source {
            Source::Android => self.config.lossless_data_dir.join("MUSIC_TOKEN"),
            Source::Browser => self.config.token_file.clone(),
        }
    }

    fn source_present(&self, source: Source) -> bool {
        // a regular file, not a link to one, with something in it
        fs::symlink_metadata(self.token_file(source)).is_ok_and(|info| info.file_type().is_file() && info.len() > 0)
    }

    /// The sign-ins present on this machine, in the order requests use them: "android" (the
    /// native sign-in of the lossless route) before "browser" (the saved browser session);
    /// `AMUI_SIGNIN=browser` reverses that. A sign-in Apple rejected while it accepted the other
    /// one goes last.
    pub fn token_sources(&self) -> Vec<Source> {
        let mut order = Source::ALL.to_vec();
        if self.config.prefer_browser_signin {
            order.reverse();
        }
        let present: Vec<Source> = order.into_iter().filter(|source| self.source_present(*source)).collect();
        let demoted = self.demoted.lock().unwrap();
        let (last, first): (Vec<Source>, Vec<Source>) =
            present.into_iter().partition(|source| demoted.contains(source));
        first.into_iter().chain(last).collect()
    }

    /// The sign-in requests use first, or `None` when this machine has none.
    pub fn token_source(&self) -> Option<Source> {
        self.token_sources().first().copied()
    }

    /// Use the other sign-in first from now on (until `reset_token_sources()` or a restart).
    pub fn demote_source(&self, source: Source) {
        self.demoted.lock().unwrap().insert(source);
    }

    /// Forget which sign-in Apple rejected (after a new sign-in).
    pub fn reset_token_sources(&self) {
        self.demoted.lock().unwrap().clear();
    }

    /// True when this machine holds a sign-in (native or browser); says nothing about its
    /// validity.
    pub fn has_saved_session(&self) -> bool {
        !self.token_sources().is_empty()
    }

    /// The music user token Apple's Android libraries keep in the private lossless runtime.
    fn read_android_token(&self) -> Result<String, WebError> {
        let path = self.token_file(Source::Android);
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|_| WebError::auth(NO_SIGN_IN, None))?;
        let unreadable = || WebError::auth("The saved sign-in could not be read; sign in again", None);
        let info = file.metadata().map_err(|_| unreadable())?;
        let folder = path.parent().and_then(|parent| fs::metadata(parent).ok()).ok_or_else(unreadable)?;
        let user = unsafe { libc::getuid() };
        // a private folder is enough: Apple's libraries choose the file's mode
        let private = info.file_type().is_file()
            && info.uid() == user
            && folder.uid() == user
            && !(info.mode() & 0o077 != 0 && folder.mode() & 0o077 != 0);
        if !private {
            return Err(WebError::auth(
                "The saved sign-in is not a private file (mode 0600 in a 0700 folder); sign in again",
                None,
            ));
        }
        let mut data = Vec::new();
        (&mut file).take(TOKEN_MAX_BYTES as u64 + 2).read_to_end(&mut data).map_err(|_| unreadable())?;
        let text = String::from_utf8_lossy(&data);
        validate_user_token(text.trim())
            .map(str::to_owned)
            .map_err(|_| WebError::auth("The saved sign-in is not usable; sign in again", None))
    }

    /// The saved browser session's cookie value.
    fn read_browser_token(&self) -> Result<String, WebError> {
        let path = self.token_file(Source::Browser);
        let unreadable = || WebError::auth("The saved browser session could not be read; sign in again", None);
        let link =
            fs::symlink_metadata(&path).map_err(|_| WebError::auth("No saved browser session; sign in again", None))?;
        let info = fs::metadata(&path).map_err(|_| unreadable())?;
        if link.file_type().is_symlink() || info.mode() & 0o077 != 0 {
            return Err(WebError::auth("Saved token must be a private file with mode 0600", None));
        }
        let text = fs::read_to_string(&path).map_err(|_| unreadable())?;
        validate_user_token(text.trim()).map(str::to_owned).map_err(|reason| WebError::auth(reason, None))
    }

    fn read_token(&self, source: Source) -> Result<String, WebError> {
        match source {
            Source::Android => self.read_android_token(),
            Source::Browser => self.read_browser_token(),
        }
    }

    /// (sign-in, token) for the next authenticated request: the named sign-in, else the first
    /// usable one outside `exclude`. The token is never printed or logged.
    fn credentials(&self, source: Option<Source>, exclude: &[Source]) -> Result<(Source, String), WebError> {
        if let Some(source) = source {
            return Ok((source, self.read_token(source)?));
        }
        let mut failure = None;
        for candidate in self.token_sources() {
            if exclude.contains(&candidate) {
                continue;
            }
            match self.read_token(candidate) {
                Ok(token) => return Ok((candidate, token)),
                Err(error) => failure = failure.or(Some(error)),
            }
        }
        Err(failure.unwrap_or_else(|| WebError::auth(NO_SIGN_IN, None)))
    }

    /// The music user token of the sign-in in use. Never print or log the result.
    pub fn user_token(&self) -> Result<String, WebError> {
        self.credentials(None, &[]).map(|(_, token)| token)
    }

    fn network_failure(host: &str, error: NetError) -> WebError {
        match error {
            NetError::Timeout => WebError::other(format!("Network request to {host} timed out")),
            NetError::Failed(reason) => WebError::other(format!("Network request to {host} failed ({reason})")),
        }
    }
}

impl Http for Web {
    fn request(&self, call: &Call) -> Result<Vec<u8>, WebError> {
        let mut url = call.url.to_string();
        let mut rejected: Vec<Source> = Vec::new(); // sign-ins Apple answered 401/403 to, for this request
        let mut rejection: Option<WebError> = None;
        let mut developer_renewed = false;
        let mut redirects = 0;
        // at most: two sign-ins, one renewed developer token, or seven requests along redirects
        for _ in 0..10 {
            let (host, target) = checked_url(&url, call.auth)?;
            let mut headers: Vec<(&str, String)> =
                vec![("User-Agent", USER_AGENT.to_string()), ("Accept", call.accept.to_string())];
            let mut used = None;
            let mut developer = String::new();
            if call.auth {
                let (source, token) = match self.credentials(call.source, &rejected) {
                    Ok(found) => found,
                    // the other sign-in is unusable: Apple's answer is the news
                    Err(error) => return Err(rejection.unwrap_or(error)),
                };
                used = Some(source);
                developer = self.developer_token(false)?;
                headers.push(("Origin", "https://music.apple.com".to_string()));
                headers.push(("Referer", "https://music.apple.com/".to_string()));
                headers.push(("Authorization", format!("Bearer {developer}")));
                headers.push(("Cookie", format!("media-user-token={token}")));
            }
            if let Some(content_type) = call.content_type {
                headers.push(("Content-Type", content_type.to_string()));
            }
            let exchange = Exchange {
                method: call.method,
                url: &target,
                headers: &headers,
                body: call.body,
                timeout: Duration::from_secs_f64(call.timeout.max(0.001)),
            };
            let (head, mut body) =
                self.transport.open(&exchange).map_err(|error| Web::network_failure(&host, error))?;
            let code = head.status;
            if (200..300).contains(&code) {
                let mut data = Vec::new();
                body.take(call.max_bytes as u64 + 1).read_to_end(&mut data).map_err(|error| {
                    let timed_out =
                        matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock);
                    let reason =
                        if timed_out { NetError::Timeout } else { NetError::Failed(format!("{:?}", error.kind())) };
                    Web::network_failure(&host, reason)
                })?;
                if data.len() > call.max_bytes {
                    return Err(WebError::other(format!("{host} response exceeded the size limit")));
                }
                for source in rejected {
                    self.demote_source(source); // Apple accepted the other sign-in: use it first from now on
                }
                return Ok(data);
            }
            if matches!(code, 301 | 302 | 303 | 307 | 308)
                && !call.auth
                && let Some(location) = head.location.as_deref()
            {
                // anonymous redirects only; credentials are never replayed
                redirects += 1;
                if redirects >= 7 {
                    break;
                }
                url = util::urljoin(&url, location).unwrap_or_else(|| location.to_string());
                continue;
            }
            if matches!(code, 401 | 403) && !call.auth {
                // no credentials were sent, so no sign-in was rejected (an expired manifest
                // link, a page that is not public)
                return Err(WebError::other(format!("{host} returned HTTP {code}")));
            }
            if code == 403
                && let Some((refusal, title)) = apple_refusal(&mut body)
                && !SESSION_REFUSALS.contains(&refusal.as_str())
            {
                // Apple refuses the item, not the sign-in: no second attempt, nobody is demoted
                let said = if title.is_empty() { refusal.clone() } else { title };
                return Err(if refusal == NOT_ENTITLED {
                    WebError::NotEntitled(format!("{host} does not let this account have this item (HTTP 403: {said})"))
                } else {
                    WebError::other(format!("{host} refused the request (HTTP 403: {said})"))
                });
            }
            if code == 401 && call.auth && !developer_renewed {
                // 401 is Apple's answer to a developer token it no longer accepts (a rejected
                // user token is a 403): fetch the current one and ask again, once
                developer_renewed = true;
                if self.developer_token(true).is_ok_and(|renewed| renewed != developer) {
                    continue;
                }
            }
            if code == 401 || code == 403 {
                let failure = WebError::auth(
                    format!("{host} rejected the session (HTTP {code}); reconnect to Apple Music"),
                    Some(code),
                );
                if let (Some(used), None) = (used, call.source) {
                    rejected.push(used);
                    if rejection.is_none() && self.token_sources().iter().any(|source| !rejected.contains(source)) {
                        rejection = Some(failure);
                        continue; // the other sign-in may still be valid: one more attempt with it
                    }
                }
                return Err(rejection.unwrap_or(failure));
            }
            if code == 404 {
                return Err(WebError::NotFound(format!("{host} returned HTTP 404")));
            }
            if code == 429 {
                let wait = head.retry_after.as_deref().and_then(|value| value.trim().parse::<f64>().ok());
                return Err(WebError::rate_limited(&host, wait));
            }
            return Err(WebError::other(format!("{host} returned HTTP {code}")));
        }
        Err(WebError::other("Too many redirects"))
    }

    fn download(
        &self,
        url: &str,
        destination: &Path,
        max_bytes: u64,
        mut progress: Option<Progress>,
        cancel: Option<&Event>,
        timeout: f64,
    ) -> Result<(u64, String), WebError> {
        let (host, target) = checked_url(url, false)?;
        // the size of what arrives is compared with Content-Length: no transfer compression
        let headers = [
            ("User-Agent", USER_AGENT.to_string()),
            ("Accept", "*/*".to_string()),
            ("Accept-Encoding", "identity".to_string()),
        ];
        if let Some(parent) = destination.parent() {
            util::create_private_dirs(parent).map_err(|_| WebError::other("Media file could not be written"))?;
        }
        let exchange = Exchange {
            method: "GET",
            url: &target,
            headers: &headers,
            body: None,
            timeout: Duration::from_secs_f64(timeout.max(0.001)),
        };
        let failed = |error: NetError| match error {
            NetError::Timeout => WebError::other(format!("Media download from {host} failed (timed out)")),
            NetError::Failed(reason) => WebError::other(format!("Media download from {host} failed ({reason})")),
        };
        let (head, mut body) = self.transport.open(&exchange).map_err(failed)?;
        if head.status != 200 {
            return Err(if (200..300).contains(&head.status) {
                WebError::other(format!("{host} did not return a complete file"))
            } else {
                WebError::other(format!("{host} returned HTTP {}; the manifest may have expired", head.status))
            });
        }
        let expected = head.content_length;
        if expected.is_some_and(|length| length > max_bytes) {
            return Err(WebError::other("Media file exceeds the download limit"));
        }
        let mut output =
            util::create_private_new(destination).map_err(|_| WebError::other("Media file could not be written"))?;
        let mut transfer = || -> Result<(u64, String), WebError> {
            let mut total = 0u64;
            let mut digest = Sha256::new();
            let mut chunk = vec![0u8; 1024 * 1024];
            loop {
                // fill a whole megabyte (or reach the end) before reporting
                let mut filled = 0;
                while filled < chunk.len() {
                    match body.read(&mut chunk[filled..]) {
                        Ok(0) => break,
                        Ok(read) => filled += read,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            let timed_out =
                                matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock);
                            return Err(failed(if timed_out {
                                NetError::Timeout
                            } else {
                                NetError::Failed(format!("{:?}", error.kind()))
                            }));
                        }
                    }
                }
                if filled == 0 {
                    break;
                }
                if cancel.is_some_and(Event::is_set) {
                    return Err(WebError::Cancelled("Download cancelled".into()));
                }
                total += filled as u64;
                if total > max_bytes {
                    return Err(WebError::other("Media file exceeded the download limit"));
                }
                output.write_all(&chunk[..filled]).map_err(|_| WebError::other("Media file could not be written"))?;
                digest.update(&chunk[..filled]);
                if let Some(report) = progress.as_mut() {
                    report(total, expected);
                }
            }
            output
                .flush()
                .and_then(|_| output.sync_all())
                .map_err(|_| WebError::other("Media file could not be written"))?;
            if expected.is_some_and(|length| length != total) {
                return Err(WebError::other("Media transfer length did not match Content-Length"));
            }
            let hex = digest.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
            Ok((total, hex))
        };
        let result = transfer();
        if result.is_err() {
            let _ = fs::remove_file(destination);
        }
        result
    }

    fn token_sources(&self) -> Vec<Source> {
        Web::token_sources(self)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A stand-in network for tests of this crate.
    use super::*;
    use std::collections::HashMap;

    pub const ANDROID_TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAandroid";
    pub const BROWSER_TOKEN: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBbrowser";

    /// What a fake server answers with.
    #[derive(Clone, Default)]
    pub struct Answer {
        pub status: u16,
        pub body: Vec<u8>,
        pub location: Option<String>,
        pub retry_after: Option<String>,
        pub content_length: Option<u64>,
    }

    impl Answer {
        pub fn ok(body: &[u8]) -> Answer {
            Answer { status: 200, body: body.to_vec(), ..Answer::default() }
        }

        pub fn status(status: u16) -> Answer {
            Answer { status, ..Answer::default() }
        }
    }

    /// One request as the fake network saw it.
    #[derive(Debug, Clone)]
    pub struct Seen {
        pub method: String,
        pub url: String,
        pub headers: HashMap<String, String>,
        pub body: Option<Vec<u8>>,
    }

    impl Seen {
        /// The token the request carried in its cookie.
        pub fn token(&self) -> Option<String> {
            self.headers.get("Cookie").and_then(|cookie| cookie.split_once('=')).map(|(_, token)| token.to_string())
        }
    }

    type Responder = Box<dyn Fn(&Seen) -> Result<Answer, NetError> + Send + Sync>;

    pub struct FakeTransport {
        respond: Responder,
        pub seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl FakeTransport {
        pub fn new(
            respond: impl Fn(&Seen) -> Result<Answer, NetError> + Send + Sync + 'static,
        ) -> (FakeTransport, Arc<Mutex<Vec<Seen>>>) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            (FakeTransport { respond: Box::new(respond), seen: seen.clone() }, seen)
        }
    }

    impl Transport for FakeTransport {
        fn open(&self, exchange: &Exchange) -> Result<(Head, Box<dyn Read>), NetError> {
            let seen = Seen {
                method: exchange.method.to_string(),
                url: exchange.url.to_string(),
                headers: exchange.headers.iter().map(|(key, value)| (key.to_string(), value.clone())).collect(),
                body: exchange.body.map(<[u8]>::to_vec),
            };
            self.seen.lock().unwrap().push(seen.clone());
            let answer = (self.respond)(&seen)?;
            let head = Head {
                status: answer.status,
                location: answer.location,
                retry_after: answer.retry_after,
                content_length: answer.content_length,
            };
            Ok((head, Box::new(std::io::Cursor::new(answer.body))))
        }
    }

    /// A scratch am-ui folder with its configuration.
    pub fn scratch_config() -> (tempfile::TempDir, Arc<Config>) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("am-ui");
        fs::create_dir_all(root.join("workspace")).unwrap();
        let config = Arc::new(Config::new(root, &dir.path().join("home"), &HashMap::new()));
        config.ensure_dirs().unwrap();
        (dir, config)
    }

    /// Write a sign-in's token file the way the real ones are kept.
    pub fn sign_in(config: &Config, source: Source, token: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = match source {
            Source::Android => config.lossless_data_dir.join("MUSIC_TOKEN"),
            Source::Browser => config.token_file.clone(),
        };
        util::create_private_dirs(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("{token}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const URL: &str = "https://amp-api.music.apple.com/v1/me/library/songs";

    struct Fixture {
        _dir: tempfile::TempDir,
        config: Arc<Config>,
        web: Web,
        seen: Arc<Mutex<Vec<Seen>>>,
        verdicts: Arc<Mutex<std::collections::HashMap<String, u16>>>,
    }

    impl Fixture {
        /// A network that answers by the cookie a request carries.
        fn new(android: u16, browser: u16) -> Fixture {
            let (dir, config) = scratch_config();
            let verdicts = Arc::new(Mutex::new(std::collections::HashMap::from([
                (ANDROID_TOKEN.to_string(), android),
                (BROWSER_TOKEN.to_string(), browser),
            ])));
            let known = verdicts.clone();
            let (transport, seen) = FakeTransport::new(move |request| {
                let status =
                    request.token().and_then(|token| known.lock().unwrap().get(&token).copied()).unwrap_or(500);
                Ok(if status == 200 {
                    Answer::ok(br#"{"data": [{"id": "x"}], "next": "/more"}"#)
                } else {
                    Answer::status(status)
                })
            });
            let web = Web::with_transport(config.clone(), Box::new(transport));
            web.preset_developer_token("developer", util::unix_now() + 90.0 * 86400.0);
            Fixture { _dir: dir, config, web, seen, verdicts }
        }

        fn sign_in(&self, sources: &[Source]) {
            for source in sources {
                let token = if *source == Source::Android { ANDROID_TOKEN } else { BROWSER_TOKEN };
                sign_in(&self.config, *source, token);
            }
        }

        fn tokens(&self) -> Vec<String> {
            self.seen.lock().unwrap().iter().filter_map(Seen::token).collect()
        }
    }

    #[test]
    fn order() {
        let fixture = Fixture::new(200, 200);
        let web = &fixture.web;
        assert_eq!((web.token_sources(), web.token_source(), web.has_saved_session()), (vec![], None, false));
        fixture.sign_in(&[Source::Browser]);
        assert_eq!(web.token_sources(), [Source::Browser]);
        fixture.sign_in(&[Source::Android]);
        assert_eq!(web.token_sources(), [Source::Android, Source::Browser]);
        web.demote_source(Source::Android);
        assert_eq!(web.token_sources(), [Source::Browser, Source::Android]);
        web.reset_token_sources();
        assert_eq!(web.token_source(), Some(Source::Android));
    }

    #[test]
    fn the_environment_can_put_the_browser_sign_in_first() {
        let dir = tempfile::tempdir().unwrap();
        let env = std::collections::HashMap::from([("AMUI_SIGNIN".to_string(), "browser".to_string())]);
        let config = Arc::new(Config::new(dir.path().join("am-ui"), &dir.path().join("home"), &env));
        sign_in(&config, Source::Android, ANDROID_TOKEN);
        sign_in(&config, Source::Browser, BROWSER_TOKEN);
        let (transport, _seen) = FakeTransport::new(|_| Ok(Answer::ok(b"{}")));
        let web = Web::with_transport(config, Box::new(transport));
        assert_eq!(web.token_sources(), [Source::Browser, Source::Android]);
    }

    #[test]
    fn no_sign_in() {
        let fixture = Fixture::new(200, 200);
        let error = fixture.web.get_json(URL, &[]).unwrap_err();
        assert_eq!((error.to_string().as_str(), error.status()), (NO_SIGN_IN, None));
        assert!(error.is_auth());
        assert!(fixture.tokens().is_empty());
    }

    #[test]
    fn the_native_sign_in_is_used_first() {
        let fixture = Fixture::new(200, 200);
        fixture.sign_in(&[Source::Android, Source::Browser]);
        fixture.web.get_json(URL, &[]).unwrap();
        assert_eq!(fixture.tokens(), [ANDROID_TOKEN]);
        let seen = fixture.seen.lock().unwrap();
        assert_eq!(seen[0].headers["Authorization"], "Bearer developer");
        assert_eq!(seen[0].headers["Origin"], "https://music.apple.com");
        assert_eq!(seen[0].headers["User-Agent"], USER_AGENT);
    }

    #[test]
    fn rejected_sign_in_falls_back_once_and_is_demoted() {
        let fixture = Fixture::new(401, 200);
        fixture.sign_in(&[Source::Android, Source::Browser]);
        assert!(fixture.web.get_json(URL, &[]).unwrap().contains_key("data"));
        assert_eq!(fixture.tokens(), [ANDROID_TOKEN, BROWSER_TOKEN]);
        fixture.web.get_json(URL, &[]).unwrap();
        assert_eq!(fixture.tokens()[2..], [BROWSER_TOKEN]); // no further attempt with the rejected one
        assert_eq!(fixture.web.token_sources(), [Source::Browser, Source::Android]);
    }

    #[test]
    fn both_rejected_reports_apples_first_answer() {
        let fixture = Fixture::new(403, 401);
        fixture.sign_in(&[Source::Android, Source::Browser]);
        let error = fixture.web.get_json(URL, &[]).unwrap_err();
        assert_eq!(error.status(), Some(403));
        assert_eq!(fixture.tokens(), [ANDROID_TOKEN, BROWSER_TOKEN]);
        assert_eq!(fixture.web.token_sources(), [Source::Android, Source::Browser]); // nobody was demoted
        for secret in [ANDROID_TOKEN, BROWSER_TOKEN] {
            assert!(!error.to_string().contains(secret));
        }
    }

    #[test]
    fn single_sign_in_rejected() {
        let fixture = Fixture::new(401, 200);
        fixture.sign_in(&[Source::Android]);
        let error = fixture.web.get_json(URL, &[]).unwrap_err();
        assert_eq!((error.status(), fixture.tokens()), (Some(401), vec![ANDROID_TOKEN.to_string()]));
    }

    #[test]
    fn unusable_second_sign_in_does_not_hide_the_rejection() {
        let fixture = Fixture::new(401, 200);
        fixture.sign_in(&[Source::Android, Source::Browser]);
        fs::write(&fixture.config.token_file, "x\n").unwrap();
        let error = fixture.web.get_json(URL, &[]).unwrap_err();
        assert_eq!((error.status(), fixture.tokens()), (Some(401), vec![ANDROID_TOKEN.to_string()]));
    }

    #[test]
    fn pinned_sign_in_makes_no_second_attempt() {
        let fixture = Fixture::new(401, 200);
        fixture.sign_in(&[Source::Android, Source::Browser]);
        let error = fixture.web.get_json_with(URL, &[], true, 20.0, Some(Source::Android)).unwrap_err();
        assert!(error.is_auth());
        assert_eq!(fixture.tokens(), [ANDROID_TOKEN]);
        assert_eq!(fixture.web.token_source(), Some(Source::Android));
    }

    #[test]
    fn native_token_file_must_be_private_and_plain() {
        let fixture = Fixture::new(200, 200);
        fixture.sign_in(&[Source::Android]);
        let data = &fixture.config.lossless_data_dir;
        let token_file = data.join("MUSIC_TOKEN");
        let mode = |path: &Path, mode: u32| fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        mode(&token_file, 0o644);
        fixture.web.get_json(URL, &[]).unwrap(); // a private folder is enough (Apple's libraries choose the file mode)
        mode(data, 0o755);
        assert!(fixture.web.get_json(URL, &[]).unwrap_err().is_auth());
        mode(data, 0o700);
        fs::write(&token_file, "has spaces and \"quotes\" in it, not a token\n").unwrap();
        let error = fixture.web.get_json(URL, &[]).unwrap_err();
        assert!(error.is_auth() && !error.to_string().contains("quotes"));
        fs::remove_file(&token_file).unwrap();
        let elsewhere = fixture.config.tmp_dir.join("a-token");
        fs::write(&elsewhere, format!("{BROWSER_TOKEN}\n")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &token_file).unwrap();
        assert_eq!(fixture.web.token_sources(), []);
        assert!(fixture.web.get_json(URL, &[]).unwrap_err().is_auth());
        assert_eq!(fixture.tokens(), [ANDROID_TOKEN]);
    }

    #[test]
    fn browser_token_file_must_be_private() {
        let fixture = Fixture::new(200, 200);
        fixture.sign_in(&[Source::Browser]);
        fixture.web.get_json(URL, &[]).unwrap();
        fs::set_permissions(&fixture.config.token_file, fs::Permissions::from_mode(0o644)).unwrap();
        let error = fixture.web.get_json(URL, &[]).unwrap_err();
        assert_eq!(error.to_string(), "Saved token must be a private file with mode 0600");
        assert_eq!(fixture.tokens(), [BROWSER_TOKEN]);
    }

    #[test]
    fn token_is_read_for_every_request() {
        let fixture = Fixture::new(200, 200);
        fixture.sign_in(&[Source::Android]);
        fixture.web.get_json(URL, &[]).unwrap();
        let refreshed = format!("{}refreshed", "C".repeat(40));
        // the service renews it while it runs
        fs::write(fixture.config.lossless_data_dir.join("MUSIC_TOKEN"), format!("{refreshed}\n")).unwrap();
        fixture.verdicts.lock().unwrap().insert(refreshed.clone(), 200);
        fixture.web.get_json(URL, &[]).unwrap();
        assert_eq!(fixture.tokens(), [ANDROID_TOKEN.to_string(), refreshed]);
        assert_eq!(fixture.web.user_token().unwrap().len(), 49);
    }

    #[test]
    fn a_refused_item_is_not_a_rejected_sign_in() {
        let (_dir, config) = scratch_config();
        sign_in(&config, Source::Android, ANDROID_TOKEN);
        sign_in(&config, Source::Browser, BROWSER_TOKEN);
        let (transport, seen) = FakeTransport::new(|request| {
            let body: &[u8] = if request.url.contains("id=withdrawn") {
                br#"{"errors": [{"id": "X", "title": "Not Entitled", "detail": "about 1234567890", "status": "403", "code": "40304"}]}"#
            } else if request.url.contains("id=odd") {
                br#"{"errors": [{"title": "Something <b>else</b>", "status": "403", "code": "40399"}]}"#
            } else if request.url.contains("id=signed-out") {
                br#"{"errors": [{"title": "Invalid Music User Token", "status": "403", "code": "40301"}]}"#
            } else {
                b"<html>forbidden</html>"
            };
            Ok(Answer { status: 403, body: body.to_vec(), ..Answer::default() })
        });
        let web = Web::with_transport(config, Box::new(transport));
        web.preset_developer_token("developer", util::unix_now() + 90.0 * 86400.0);
        let assets =
            |id: &str| web.get_json(&format!("https://amp-api.music.apple.com/v1/play/assets?id={id}&kind=song"), &[]);

        let error = assets("withdrawn").unwrap_err();
        assert!(matches!(error, WebError::NotEntitled(_)) && !error.is_auth());
        assert_eq!(
            error.to_string(),
            "amp-api.music.apple.com does not let this account have this item (HTTP 403: Not Entitled)"
        );
        assert!(!error.to_string().contains("1234567890"));
        assert_eq!(seen.lock().unwrap().len(), 1); // the other sign-in is not tried
        assert_eq!(web.token_sources(), [Source::Android, Source::Browser]);

        let error = assets("odd").unwrap_err();
        assert_eq!(error.to_string(), "amp-api.music.apple.com refused the request (HTTP 403: Something belseb)");
        assert!(!error.is_auth());

        // Apple's codes for a sign-in it does not accept, and an answer without a code, stay what they were
        for id in ["signed-out", "blocked"] {
            let before = seen.lock().unwrap().len();
            let error = assets(id).unwrap_err();
            assert_eq!((error.is_auth(), error.status()), (true, Some(403)), "{id}");
            assert_eq!(seen.lock().unwrap().len() - before, 2, "{id}: both sign-ins are tried");
        }
    }

    #[test]
    fn a_refused_developer_token_is_fetched_again_once() {
        let (_dir, config) = scratch_config();
        sign_in(&config, Source::Android, ANDROID_TOKEN);
        let now = util::unix_now();
        let current = jwt("AMPWebPlay", now + 60.0 * 86400.0);
        let script = format!("token=\"{current}\";");
        let wanted = current.clone();
        let (transport, seen) = FakeTransport::new(move |request| {
            Ok(if request.url.ends_with("/us/browse") {
                Answer::ok(b"<script src=\"/assets/index~x.js\"></script>")
            } else if request.url.ends_with("/assets/index~x.js") {
                Answer::ok(script.as_bytes())
            } else if request.headers.get("Authorization").is_some_and(|value| value == &format!("Bearer {wanted}")) {
                Answer::ok(b"{\"data\": []}")
            } else {
                Answer::status(401)
            })
        });
        let web = Web::with_transport(config, Box::new(transport));
        web.preset_developer_token("stale", now + 30.0 * 86400.0);
        assert!(web.get_json(URL, &[]).unwrap().contains_key("data"));
        assert_eq!(web.developer_token(false).unwrap(), current);
        // the stale token, the two requests that find the current one, the request again
        assert_eq!(seen.lock().unwrap().len(), 4);
        assert_eq!(web.token_sources(), [Source::Android]); // the sign-in was never the problem
    }

    #[test]
    fn only_apple_hosts_over_https() {
        for url in [
            "https://music.apple.com/us/browse",
            "https://amp-api.music.apple.com/v1/me/storefront",
            "https://aod.itunes.apple.com/itunes-assets/a.m3u8?x=1",
            "https://is1-ssl.mzstatic.com/image/thumb/a.jpg",
            "https://play.itunes.apple.com:443/WebObjects/MZPlay.woa/wa/webPlayback",
        ] {
            assert!(validate_url(url, false).is_ok(), "{url}");
        }
        for url in [
            "http://music.apple.com/",
            "https://example.com/",
            "https://music.apple.com.example.com/",
            "https://evilmzstatic.com/a.jpg",
            "https://user:secret@music.apple.com/",
            "https://music.apple.com:8443/",
            "https://music.apple.com@example.com/",
            "ftp://music.apple.com/",
            "not a url",
        ] {
            assert!(validate_url(url, false).is_err(), "{url}");
        }
        assert_eq!(validate_url("https://amp-api.music.apple.com/v1/x", true).unwrap(), config::API_HOST);
        assert_eq!(validate_url(config::LICENSE_URL, true).unwrap(), config::LICENSE_HOST);
        let error = validate_url("https://aod.itunes.apple.com/a.m3u8", true).unwrap_err();
        assert!(error.to_string().contains("only be sent to Apple's API and license hosts"));
    }

    #[test]
    fn credentials_never_follow_a_redirect() {
        let (_dir, config) = scratch_config();
        sign_in(&config, Source::Android, ANDROID_TOKEN);
        let (transport, seen) = FakeTransport::new(|request| {
            Ok(if request.url.contains("/moved") {
                Answer::ok(b"#EXTM3U\n")
            } else {
                Answer { status: 302, location: Some("/moved".into()), ..Answer::default() }
            })
        });
        let web = Web::with_transport(config, Box::new(transport));
        web.preset_developer_token("developer", util::unix_now() + 90.0 * 86400.0);
        let error = web.get_json(URL, &[]).unwrap_err();
        assert_eq!(error.to_string(), "amp-api.music.apple.com returned HTTP 302");
        assert_eq!(seen.lock().unwrap().len(), 1);
        // an anonymous request follows it, still without credentials and only to allowed hosts
        assert_eq!(web.get_text("https://aod.itunes.apple.com/a/master.m3u8?sig=1").unwrap(), "#EXTM3U\n");
        let seen = seen.lock().unwrap();
        assert_eq!(seen[2].url, "https://aod.itunes.apple.com/moved");
        assert!(
            seen[1..].iter().all(|request| request.token().is_none() && !request.headers.contains_key("Authorization"))
        );
    }

    #[test]
    fn a_redirect_to_another_site_is_refused() {
        let (_dir, config) = scratch_config();
        let (transport, seen) = FakeTransport::new(|_| {
            Ok(Answer { status: 301, location: Some("https://example.com/x".into()), ..Answer::default() })
        });
        let web = Web::with_transport(config, Box::new(transport));
        let error = web.get_bytes("https://is1-ssl.mzstatic.com/image/a.jpg").unwrap_err();
        assert_eq!(error.to_string(), "Refused an unexpected URL; no token was sent");
        assert_eq!(seen.lock().unwrap().len(), 1);
        let (transport, seen) = FakeTransport::new(|request| {
            Ok(Answer { status: 302, location: Some(format!("{}x", request.url)), ..Answer::default() })
        });
        let (_dir, config) = scratch_config();
        let web = Web::with_transport(config, Box::new(transport));
        assert_eq!(web.get_text("https://music.apple.com/a").unwrap_err().to_string(), "Too many redirects");
        assert_eq!(seen.lock().unwrap().len(), 7);
    }

    #[test]
    fn answers_become_errors_without_details() {
        let (_dir, config) = scratch_config();
        let (transport, _seen) = FakeTransport::new(|request| {
            let path = request.url.rsplit('/').next().unwrap_or("");
            match path {
                "missing" => Ok(Answer::status(404)),
                "busy" => Ok(Answer { status: 429, retry_after: Some("12".into()), ..Answer::default() }),
                "broken" => Ok(Answer::status(500)),
                "expired" => Ok(Answer::status(403)),
                "slow" => Err(NetError::Timeout),
                "down" => Err(NetError::Failed("connection failed".into())),
                "list" => Ok(Answer::ok(b"[1, 2]")),
                "page" => Ok(Answer::ok(b"<html>")),
                "media" => Ok(Answer::ok(&[0x00, 0xff, 0xfe, 0x80])),
                "big" => Ok(Answer::ok(&vec![b'a'; 2048])),
                _ => Ok(Answer::ok(b"{\"ok\": true}")),
            }
        });
        let web = Web::with_transport(config, Box::new(transport));
        let get = |name: &str| web.get_json_with(&format!("https://music.apple.com/{name}"), &[], false, 20.0, None);
        assert_eq!(get("fine").unwrap()["ok"], true);
        assert_eq!(get("missing").unwrap_err(), WebError::NotFound("music.apple.com returned HTTP 404".into()));
        match get("busy").unwrap_err() {
            WebError::RateLimited { message, retry_after } => {
                assert_eq!(message, "music.apple.com rate-limited this session; retry after 12s");
                assert_eq!(retry_after, Some(12.0));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(get("broken").unwrap_err().to_string(), "music.apple.com returned HTTP 500");
        // without credentials a 403 is not a rejected sign-in
        let expired = get("expired").unwrap_err();
        assert_eq!((expired.to_string().as_str(), expired.is_auth()), ("music.apple.com returned HTTP 403", false));
        assert_eq!(get("slow").unwrap_err().to_string(), "Network request to music.apple.com timed out");
        assert_eq!(
            get("down").unwrap_err().to_string(),
            "Network request to music.apple.com failed (connection failed)"
        );
        assert_eq!(get("list").unwrap_err().to_string(), "Apple returned an unexpected JSON shape");
        assert_eq!(get("page").unwrap_err().to_string(), "Apple returned a response that is not JSON");
        assert!(matches!(web.get_text("https://music.apple.com/media").unwrap_err(), WebError::NotText(_)));
        let error = web.get_bytes_with("https://music.apple.com/big", 1024, 20.0).unwrap_err();
        assert_eq!(error.to_string(), "music.apple.com response exceeded the size limit");
        assert_eq!(web.get_bytes_with("https://music.apple.com/big", 2048, 20.0).unwrap().len(), 2048);
    }

    #[test]
    fn query_parameters_and_bodies() {
        let fixture = Fixture::new(200, 200);
        fixture.sign_in(&[Source::Android]);
        fixture.web.get_json(URL, &[("limit", "100"), ("include[library-songs]", "albums")]).unwrap();
        fixture.web.get_json(&format!("{URL}?offset=100"), &[("limit", "1")]).unwrap();
        fixture.web.post_json("https://amp-api.music.apple.com/v1/me/library/playlists", &json!({"a": [1]})).unwrap();
        let seen = fixture.seen.lock().unwrap();
        assert_eq!(seen[0].url, format!("{URL}?limit=100&include%5Blibrary-songs%5D=albums"));
        assert_eq!(seen[1].url, format!("{URL}?offset=100&limit=1"));
        assert_eq!((seen[2].method.as_str(), seen[2].body.as_deref()), ("POST", Some(br#"{"a":[1]}"#.as_slice())));
        assert_eq!(seen[2].headers["Content-Type"], "application/json");
    }

    fn jwt(issuer: &str, expires: f64) -> String {
        let encode = |value: Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string());
        format!("{}.{}.c2lnbmF0dXJl", encode(json!({"alg": "ES256"})), encode(json!({"iss": issuer, "exp": expires})))
    }

    #[test]
    fn developer_token_is_found_once_and_kept() {
        let (_dir, config) = scratch_config();
        let now = util::unix_now();
        let (current, expired, foreign) =
            (jwt("AMPWebPlay", now + 60.0 * 86400.0), jwt("AMPWebPlay", now - 5.0), jwt("Other", now + 60.0 * 86400.0));
        let script = format!("x=\"{expired}\";y=\"{foreign}\";z=\"{current}\";");
        let page = "<html><script src=\"/assets/vendor~abc.js\"></script><script type=\"module\" src='/assets/index~abc.js'></script></html>";
        let (transport, seen) = FakeTransport::new(move |request| {
            Ok(if request.url.ends_with("/us/browse") {
                Answer::ok(page.as_bytes())
            } else if request.url.ends_with("/assets/index~abc.js") {
                Answer::ok(script.as_bytes())
            } else {
                Answer::status(404)
            })
        });
        let web = Web::with_transport(config.clone(), Box::new(transport));
        assert_eq!(web.developer_token(false).unwrap(), current);
        assert_eq!(web.developer_token(false).unwrap(), current);
        assert_eq!(seen.lock().unwrap().len(), 2);
        // another start of the program reads it from the state folder
        let (transport, unused) = FakeTransport::new(|_| Ok(Answer::status(500)));
        let later = Web::with_transport(config.clone(), Box::new(transport));
        assert_eq!(later.developer_token(false).unwrap(), current);
        assert!(unused.lock().unwrap().is_empty());
        let saved = fs::metadata(config.state_dir.join("developer-token.json")).unwrap();
        assert_eq!(saved.permissions().mode() & 0o777, 0o600);
        // a forced refresh asks again; without a current token in the scripts it fails
        assert!(later.developer_token(true).is_err());
        assert_eq!(web.developer_token(true).unwrap(), current);
        assert_eq!(seen.lock().unwrap().len(), 4);
    }

    #[test]
    fn download_writes_a_private_file_and_checks_its_length() {
        let (_dir, config) = scratch_config();
        let media: Vec<u8> = (0..(2 * 1024 * 1024 + 4096)).map(|i| (i % 251) as u8).collect();
        let served = media.clone();
        let (transport, seen) = FakeTransport::new(move |request| {
            let name = request.url.rsplit('/').next().unwrap_or("");
            Ok(match name {
                "song.m4a" => Answer { content_length: Some(served.len() as u64), ..Answer::ok(&served) },
                "short.m4a" => Answer { content_length: Some(served.len() as u64 + 1), ..Answer::ok(&served) },
                "partial.m4a" => Answer { status: 206, ..Answer::ok(&served) },
                "moved.m4a" => Answer { status: 302, location: Some("/song.m4a".into()), ..Answer::default() },
                _ => Answer::status(403),
            })
        });
        let web = Web::with_transport(config.clone(), Box::new(transport));
        let folder = config.tmp_dir.join("downloads");
        let url = |name: &str| format!("https://aod.itunes.apple.com/itunes-assets/{name}");
        let mut reports = Vec::new();
        let mut progress = |done: u64, total: Option<u64>| reports.push((done, total));
        let target = folder.join("song.m4a");
        let (size, digest) =
            web.download(&url("song.m4a"), &target, config::MEDIA_MAX_BYTES, Some(&mut progress), None, 45.0).unwrap();
        assert_eq!(size, media.len() as u64);
        assert_eq!(digest, Sha256::digest(&media).iter().map(|b| format!("{b:02x}")).collect::<String>());
        assert_eq!(fs::read(&target).unwrap(), media);
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o600);
        let total = Some(media.len() as u64);
        assert_eq!(reports, [(1 << 20, total), (2 << 20, total), (media.len() as u64, total)]);
        {
            let seen = seen.lock().unwrap();
            assert!(seen[0].token().is_none() && !seen[0].headers.contains_key("Authorization"));
        }
        // an existing file is never overwritten
        assert!(web.download(&url("song.m4a"), &target, config::MEDIA_MAX_BYTES, None, None, 45.0).is_err());
        assert_eq!(fs::read(&target).unwrap(), media);

        let fails = |name: &str, max: u64, cancel: Option<&Event>| {
            let target = folder.join(name);
            let error = web.download(&url(name), &target, max, None, cancel, 45.0).unwrap_err();
            assert!(!target.exists(), "{name} left a partial file");
            error
        };
        assert_eq!(
            fails("short.m4a", config::MEDIA_MAX_BYTES, None).to_string(),
            "Media transfer length did not match Content-Length"
        );
        assert_eq!(
            fails("song.m4a.too-big", 1024, None).to_string(),
            "aod.itunes.apple.com returned HTTP 403; the manifest may have expired"
        );
        assert_eq!(
            fails("partial.m4a", config::MEDIA_MAX_BYTES, None).to_string(),
            "aod.itunes.apple.com did not return a complete file"
        );
        assert_eq!(
            fails("moved.m4a", config::MEDIA_MAX_BYTES, None).to_string(),
            "aod.itunes.apple.com returned HTTP 302; the manifest may have expired"
        );
        let cancel = Event::new();
        cancel.set();
        let target = folder.join("cancelled.m4a");
        let error =
            web.download(&url("song.m4a"), &target, config::MEDIA_MAX_BYTES, None, Some(&cancel), 45.0).unwrap_err();
        assert!(matches!(error, WebError::Cancelled(_)) && !target.exists());
        let target = folder.join("limited.m4a");
        let error = web.download(&url("song.m4a"), &target, 1024 * 1024, None, None, 45.0).unwrap_err();
        assert_eq!(error.to_string(), "Media file exceeds the download limit");
        assert!(!target.exists());
        let error =
            web.download("https://example.com/song.m4a", &folder.join("x"), 1024, None, None, 45.0).unwrap_err();
        assert_eq!(error.to_string(), "Refused an unexpected URL; no token was sent");
    }
}
