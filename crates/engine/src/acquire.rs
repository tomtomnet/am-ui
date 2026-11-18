//! Song -> decrypted Atmos .m4a (strict: no silent stereo fallback).
//!
//! Mirrors the proven workspace flow (tools/music-web-probe.py + tools/music-web-download.py):
//!
//!  1. GET /v1/play/assets?id=&kind=song&includeLicenseUrls=true&hlsEncryption=CBC&hlsProfile=enhancedHls (auth)
//!  2. master playlist: path `P\d+_<x>.m3u8` -> `P\d+_default.m3u8` (keep signed query); GET
//!     without credentials
//!  3. select an `#EXT-X-STREAM-INF` with `AUDIO="audio-atmos-*"` and "ec-3" in `CODECS`,
//!     highest `BANDWIDTH`; none -> [`AcquireError::NoAtmos`]. Variant "stereo":
//!     `AUDIO="audio-stereo-256"` (AAC-LC).
//!  4. leaf playlist: one non-prefetch Widevine `EXT-X-KEY` (`KEYFORMAT`
//!     urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed, `URI` `data:text/plain;base64,<pssh>`), one
//!     `EXT-X-MAP` media file with byte ranges
//!  5. DRM helper (`config.drm_helper`) over stdin/stdout JSON lines:
//!     ```text
//!     -> {"pssh","encrypted_file","output_file","prefetch_key"}  <- {"challenge","protocol"}
//!     POST config::LICENSE_URL {"challenge","key-system":"com.widevine.alpha","uri":<key URI>,
//!                               "adamId":<id>,"user-initiated":true} (auth)
//!     -> {"license"}                                <- {"license_accepted": true}
//!     download media (no credentials, cap config::MEDIA_MAX_BYTES) into encrypted_file
//!     -> {"decrypt": true}                          <- {"decrypted": true, "output_bytes": n}
//!     ```
//!  6. move output into the `MediaCache`; ffprobe duration; delete the encrypted file and the
//!     helper log.
//!
//! Two playlist layouts exist. Most songs: streams in named audio groups, and a stream playlist
//! that lists Apple's fixed prefetch key next to the song's key; the media file then has two
//! sample descriptions, the first encrypted with the fixed key. A song Apple encoded only as AAC:
//! streams without an audio group, a stream playlist with the song's key alone, one sample
//! description, all of it encrypted with the song's key. "prefetch_key" tells the helper which
//! layout it decrypts; a helper built before that message existed (no "protocol" 2 in its answer)
//! is refused for the second layout before any license is requested, because it would write
//! noise.
//!
//! The "lossless" variant (ALAC, which Apple serves through FairPlay only) is a different route
//! and lives in [`crate::lossless`]; `acquire()` dispatches to it. [`decode_stereo`] decodes both
//! AAC and ALAC files.
//!
//! Never log tokens, signed URLs, challenges, licenses or keys: this module keeps them in local
//! variables only and its errors carry fixed strings and HTTP status codes. Errors of the HTTP
//! layer ([`WebError`]: no sign-in, rate limit, cancellation...) pass through unchanged as
//! [`AcquireError::Web`]; cancellation is `WebError::Cancelled`.
//!
//! Beyond the stub contract: [`probe_info`], the pure parsing helpers ([`master_url_for`],
//! [`select_atmos`], [`select_stereo`], [`parse_leaf`], [`LeafInfo`]) used by tests, and the
//! `has_atmos()` answer cache (`forget_atmos()`).
//!
//! Deliberate differences from `amui/acquire.py`:
//!
//! * What Python kept in module globals lives in an [`Acquirer`]: the configuration, the HTTP
//!   layer, the lossless route and the `has_atmos()` answers. `acquirer.acquire(...)`,
//!   `has_atmos()` and `forget_atmos()` are its methods, and the answers live as long as the
//!   `Acquirer` (the engine holds one), not as long as the process.
//! * One error type: [`AcquireError`] with `NoAtmos` and `License` for Python's subclasses,
//!   `Lossless` for the lossless route's errors and `Web` for the HTTP layer's.
//! * The playlist helpers Python borrowed from the workspace's probe module (`attributes`, the
//!   Atmos stream list) are part of this module.
//! * Stream variants, the leaf summary and the ffprobe summary are structs ([`StreamVariant`],
//!   [`LeafInfo`], [`ProbeInfo`]); the `Debug` output of the first two leaves out URLs and key
//!   material.
//! * The messages to the DRM helper are compact JSON lines (Python put a space after `:` and
//!   `,`); the helper reads both alike. Its answers are read by a thread, and an answer that is
//!   not UTF-8 is "DRM helper sent an invalid message" (Python raised a decoding error).
//! * `catalog_id` is text (Python also took a number).
//! * Operating-system errors that Python let through as `OSError` (the helper's log file, the
//!   size of a file, the cache's index) are `AcquireError::Other` with a short reason.
//! * An ffprobe answer that is not a JSON object gives the empty summary (Python raised), and
//!   output of ffmpeg or ffprobe that is not UTF-8 is read with replacement characters.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use base64::Engine as _;
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::cache::{CacheError, MediaCache};
use crate::config::{self, Config};
use crate::lossless::LosslessError;
use crate::render::{
    last_line, os_error_name, private_umask, returncode, seconds, splitlines, terminate, wait_timeout,
};
use crate::types::AcquireResult;
use crate::util::{self, Event};
use crate::web::{self, Http, WebError};

/// Called with (phase, fraction 0..1 or `None`).
pub type ProgressFn<'a> = &'a mut dyn FnMut(&str, Option<f64>);

/// Why a song could not be acquired; the messages are safe to show and log.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AcquireError {
    /// Python's plain `AcquireError`.
    #[error("{0}")]
    Other(String),
    /// Apple advertised no Dolby Atmos (ec-3) variant for this song and account.
    #[error("{0}")]
    NoAtmos(String),
    /// The license exchange failed.
    #[error("{0}")]
    License(String),
    /// The lossless route cannot deliver this song; the AAC variant is the truthful fallback.
    #[error(transparent)]
    Lossless(#[from] LosslessError),
    /// A failure of the HTTP layer, unchanged: no sign-in, rate limit, cancellation...
    #[error(transparent)]
    Web(#[from] WebError),
}

impl AcquireError {
    pub fn other(message: impl Into<String>) -> AcquireError {
        AcquireError::Other(message.into())
    }

    pub fn is_no_atmos(&self) -> bool {
        matches!(self, AcquireError::NoAtmos(_))
    }

    /// Any failure of the lossless route, for which the next version down is the right fallback.
    pub fn is_lossless(&self) -> bool {
        matches!(self, AcquireError::Lossless(_))
    }

    /// The song has no ALAC rendition at all.
    pub fn is_no_lossless(&self) -> bool {
        matches!(self, AcquireError::Lossless(LosslessError::NoLossless(_)))
    }

    /// The song does not have the ALAC rendition that was asked for: the route is fine.
    pub fn is_no_rendition(&self) -> bool {
        matches!(self, AcquireError::Lossless(error) if error.is_no_rendition())
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, AcquireError::Web(WebError::Cancelled(_)))
    }
}

impl From<CacheError> for AcquireError {
    fn from(error: CacheError) -> AcquireError {
        AcquireError::Other(error.to_string())
    }
}

pub const ASSETS_URL: &str = "https://amp-api.music.apple.com/v1/play/assets";
pub const WIDEVINE_KEYFORMAT: &str = "urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed";
/// Apple's fixed prefetch key (key id "s1/e1"), listed in every leaf next to the song's own key.
/// The same constant is used by the workspace's tools/music-web-download.py; it is not a secret.
pub const PREFETCH_KEY_URI: &str =
    "data:text/plain;base64,AAAAOHBzc2gAAAAA7e+LqXnWSs6jyCfc1R0h7QAAABgSEAAAAAAAAAAAczEvZTEgICBI88aJmwY=";
pub const DATA_URI_PREFIX: &str = "data:text/plain;base64,";
pub const STEREO_GROUP: &str = "audio-stereo-256";
pub const HELPER_TIMEOUT_S: f64 = 30.0;
pub const DECRYPT_TIMEOUT_S: f64 = 120.0;
pub const FFPROBE_TIMEOUT_S: f64 = 60.0;
const FFMPEG_TIMEOUT_S: f64 = 600.0;
const DOWNLOAD_TIMEOUT_S: f64 = 45.0;

/// The helper names what it understands next to its challenge ("protocol"; absent = 1). From 2
/// on it reads "prefetch_key" and can decrypt a file whose playlist lists the song's key alone.
pub const HELPER_PROTOCOL_SINGLE_KEY: i64 = 2;
pub const HELPER_OUT_OF_DATE: &str =
    "this song needs a newer build of the DRM helper: run ./setup.sh in the am-ui folder, then start am-ui again";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn pattern(cell: &'static OnceLock<Regex>, text: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(text).expect("a valid pattern"))
}

// --- pure helpers (unit-tested) ---------------------------------------------------

/// Where the path of a URL starts and ends in its text.
pub(crate) fn path_span(url: &str) -> (usize, usize) {
    let end = url.find(['?', '#']).unwrap_or(url.len());
    let start = match url[..end].find("://") {
        // the host ends at the first slash after it
        Some(scheme) => url[scheme + 3..end].find('/').map_or(end, |slash| scheme + 3 + slash),
        None => 0,
    };
    (start, end)
}

/// AMDL's manifest selection: `P<n>_<x>.m3u8` -> `P<n>_default.m3u8`, signed query retained.
pub fn master_url_for(asset_url: &str) -> String {
    static NAME: OnceLock<Regex> = OnceLock::new();
    let (start, end) = path_span(asset_url);
    let path = pattern(&NAME, r"(P\d+)_([^/]+)(\.m3u8)").replace_all(&asset_url[start..end], "${1}_default${3}");
    format!("{}{path}{}", &asset_url[..start], &asset_url[end..])
}

/// The attributes of one playlist tag: `KEY=value` or `KEY="value"`, separated by commas.
pub fn attributes(line: &str) -> HashMap<String, String> {
    static ATTRIBUTE: OnceLock<Regex> = OnceLock::new();
    pattern(&ATTRIBUTE, r#"([A-Z0-9-]+)=(?:"([^"]*)"|([^,]*))(?:,|$)"#)
        .captures_iter(line)
        .map(|found| {
            let value = found.get(2).or(found.get(3)).map_or("", |value| value.as_str());
            (found[1].to_string(), value.to_string())
        })
        .collect()
}

/// One `#EXT-X-STREAM-INF` of a master playlist with its resolved URL.
#[derive(Clone, PartialEq, Eq)]
pub struct StreamVariant {
    /// The `AUDIO` attribute; empty when the stream is in no audio group.
    pub audio_group: String,
    pub codecs: String,
    /// The `BANDWIDTH` attribute as the playlist spells it.
    pub bandwidth: Option<String>,
    /// The stream's playlist. It may be signed: it stays out of logs and errors.
    pub url: String,
}

impl StreamVariant {
    /// `{"audio_group", "codecs", "bandwidth"}`, what an [`AcquireResult`] says about its
    /// variant.
    pub fn to_map(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("audio_group".to_string(), json!(self.audio_group));
        map.insert("codecs".to_string(), json!(self.codecs));
        map.insert("bandwidth".to_string(), json!(self.bandwidth));
        map
    }

    fn rank(&self) -> i64 {
        self.bandwidth.as_deref().and_then(|text| text.trim().parse().ok()).unwrap_or(0)
    }
}

impl std::fmt::Debug for StreamVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamVariant")
            .field("audio_group", &self.audio_group)
            .field("codecs", &self.codecs)
            .field("bandwidth", &self.bandwidth)
            .finish_non_exhaustive()
    }
}

/// The first of the items with the highest rank (what Python's `max(items, key=...)` returns).
pub(crate) fn first_max<T, K: PartialOrd>(items: impl IntoIterator<Item = T>, rank: impl Fn(&T) -> K) -> Option<T> {
    let mut best: Option<(K, T)> = None;
    for item in items {
        let value = rank(&item);
        if best.as_ref().is_none_or(|(top, _)| value > *top) {
            best = Some((value, item));
        }
    }
    best.map(|(_, item)| item)
}

/// `base` + `reference` as a browser resolves them; the reference itself when either is not a
/// URL (it then fails the host check).
fn join(base: &str, reference: &str) -> String {
    util::urljoin(base, reference).unwrap_or_else(|| reference.to_string())
}

/// The non-blank lines of a master playlist, which starts with `#EXTM3U`.
fn master_lines(master: &str) -> Result<Vec<&str>, AcquireError> {
    let lines: Vec<&str> = splitlines(master).into_iter().map(str::trim).filter(|line| !line.is_empty()).collect();
    if lines.first() != Some(&"#EXTM3U") {
        return Err(AcquireError::other("Asset response was not an HLS playlist"));
    }
    Ok(lines)
}

/// The line after a stream tag: its playlist.
fn stream_uri<'a>(lines: &[&'a str], index: usize) -> Option<&'a str> {
    lines.get(index + 1).copied().filter(|line| !line.starts_with('#'))
}

/// Every stream in an `audio-atmos-*` group whose codecs include ec-3.
fn atmos_variants(master: &str, master_url: &str) -> Result<Vec<StreamVariant>, AcquireError> {
    let lines = master_lines(master)?;
    let mut variants = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(tag) = line.strip_prefix("#EXT-X-STREAM-INF:") else { continue };
        let mut attrs = attributes(tag);
        let audio_group = attrs.remove("AUDIO").unwrap_or_default();
        let codecs = attrs.remove("CODECS").unwrap_or_default();
        if !audio_group.starts_with("audio-atmos-") || !codecs.split(',').any(|codec| codec == "ec-3") {
            continue;
        }
        let Some(uri) = stream_uri(&lines, index) else {
            return Err(AcquireError::other("Atmos variant is missing its playlist URL"));
        };
        let url = join(master_url, uri);
        web::validate_url(&url, false).map_err(|error| AcquireError::other(error.to_string()))?;
        variants.push(StreamVariant { audio_group, codecs, bandwidth: attrs.remove("BANDWIDTH"), url });
    }
    Ok(variants)
}

/// Highest-bandwidth ec-3 Atmos variant, or [`AcquireError::NoAtmos`].
pub fn select_atmos(master: &str, master_url: &str) -> Result<StreamVariant, AcquireError> {
    first_max(atmos_variants(master, master_url)?, StreamVariant::rank).ok_or_else(|| {
        AcquireError::NoAtmos("Apple advertises no Dolby Atmos (ec-3) variant for this song".to_string())
    })
}

/// Every `#EXT-X-STREAM-INF` of a master playlist with its resolved URL.
pub fn stream_variants(master: &str, master_url: &str) -> Result<Vec<StreamVariant>, AcquireError> {
    let lines = master_lines(master)?;
    let mut variants = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(tag) = line.strip_prefix("#EXT-X-STREAM-INF:") else { continue };
        let mut attrs = attributes(tag);
        let Some(uri) = stream_uri(&lines, index) else {
            return Err(AcquireError::other("A stream variant is missing its playlist URL"));
        };
        variants.push(StreamVariant {
            audio_group: attrs.remove("AUDIO").unwrap_or_default(),
            codecs: attrs.remove("CODECS").unwrap_or_default(),
            bandwidth: attrs.remove("BANDWIDTH"),
            url: join(master_url, uri),
        });
    }
    Ok(variants)
}

fn only_aac(variant: &StreamVariant) -> bool {
    let mut codecs = variant.codecs.split(',').map(str::trim).filter(|codec| !codec.is_empty()).peekable();
    codecs.peek().is_some() && codecs.all(|codec| codec.starts_with("mp4a"))
}

/// `AUDIO="audio-stereo-256"` (AAC-LC); fallback: a plain `audio-stereo-*` AAC group, then any
/// `audio-stereo-*`.
///
/// A song Apple encoded only as AAC has a master without audio groups: its streams (256/128 kbps
/// AAC-LC, 64 kbps HE-AAC) carry no `AUDIO` attribute. Those come last, AAC-LC before HE-AAC.
pub fn select_stereo(master: &str, master_url: &str) -> Result<StreamVariant, AcquireError> {
    static PLAIN: OnceLock<Regex> = OnceLock::new();
    let plain_group = pattern(&PLAIN, r"^audio-stereo-\d+$");
    let variants = stream_variants(master, master_url)?;
    let candidates: Vec<&StreamVariant> = variants
        .iter()
        .filter(|variant| variant.audio_group.starts_with("audio-stereo-"))
        .filter(|variant| variant.codecs.split(',').any(|codec| codec.starts_with("mp4a")))
        .collect();
    let ungrouped: Vec<&StreamVariant> =
        variants.iter().filter(|variant| variant.audio_group.is_empty() && only_aac(variant)).collect();
    let pools: [Vec<&StreamVariant>; 5] = [
        candidates.iter().copied().filter(|variant| variant.audio_group == STEREO_GROUP).collect(),
        candidates.iter().copied().filter(|variant| plain_group.is_match(&variant.audio_group)).collect(),
        candidates.clone(),
        ungrouped.iter().copied().filter(|variant| variant.codecs.contains("mp4a.40.2")).collect(),
        ungrouped.clone(),
    ];
    for pool in pools {
        if let Some(chosen) = first_max(pool, |variant| variant.rank()) {
            web::validate_url(&chosen.url, false)?;
            return Ok(chosen.clone());
        }
    }
    Err(AcquireError::other("Apple advertises no AAC stereo variant for this song"))
}

/// What a stream playlist says about its key and its media file.
#[derive(Clone, PartialEq, Eq)]
pub struct LeafInfo {
    /// Full data: URI, sent back to Apple as the license "uri".
    pub key_uri: String,
    /// Base64 payload for the DRM helper.
    pub pssh: String,
    /// The single byte-range media file.
    pub media_url: String,
    pub segments: usize,
    /// The playlist also lists Apple's fixed prefetch key (see the module text).
    pub prefetch: bool,
}

impl std::fmt::Debug for LeafInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeafInfo")
            .field("segments", &self.segments)
            .field("prefetch", &self.prefetch)
            .finish_non_exhaustive()
    }
}

/// Standard base64 with padding, as Python's `b64decode(text, validate=True)` accepts it.
fn is_base64(text: &str) -> bool {
    use base64::engine::{GeneralPurpose, GeneralPurposeConfig};
    const PYTHON: GeneralPurpose = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
    );
    PYTHON.decode(text).is_ok()
}

/// Exactly one non-prefetch Widevine key and one media file (byte ranges), as the inspected AMDL
/// flow uses.
///
/// `prefetch` records whether the fixed prefetch key is listed too: it decides which key the
/// first sample description of the media file is encrypted with.
pub fn parse_leaf(leaf: &str, leaf_url: &str) -> Result<LeafInfo, AcquireError> {
    if !leaf.trim_start().starts_with("#EXTM3U") {
        return Err(AcquireError::other("Selected stream response was not an HLS playlist"));
    }
    let mut keys: Vec<String> = Vec::new();
    let mut prefetch = false;
    let mut maps: HashSet<String> = HashSet::new();
    let mut media: HashSet<String> = HashSet::new();
    let mut segments = 0;
    for line in splitlines(leaf).into_iter().map(str::trim) {
        if let Some(tag) = line.strip_prefix("#EXT-X-KEY:") {
            let mut attrs = attributes(tag);
            if attrs.get("KEYFORMAT").map(String::as_str) == Some(WIDEVINE_KEYFORMAT) {
                let uri = attrs.remove("URI").unwrap_or_default();
                if uri == PREFETCH_KEY_URI {
                    prefetch = true;
                } else if !keys.contains(&uri) {
                    keys.push(uri);
                }
            }
        } else if let Some(tag) = line.strip_prefix("#EXT-X-MAP:") {
            maps.insert(join(leaf_url, attributes(tag).get("URI").map_or("", String::as_str)));
        } else if !line.is_empty() && !line.starts_with('#') {
            media.insert(join(leaf_url, line));
            segments += 1;
        }
    }
    let key_uri = match keys.as_slice() {
        [key_uri] if key_uri.starts_with(DATA_URI_PREFIX) => key_uri.clone(),
        _ => {
            return Err(AcquireError::other(
                "Expected one non-prefetch Widevine key in the playlist; no license request made",
            ));
        }
    };
    if media.len() != 1 || maps != media {
        return Err(AcquireError::other("Expected one byte-range media file, as used by the inspected AMDL flow"));
    }
    let media_url = media.into_iter().next().unwrap_or_default();
    web::validate_url(&media_url, false)?;
    let pssh = key_uri[DATA_URI_PREFIX.len()..].to_string();
    if !is_base64(&pssh) {
        return Err(AcquireError::other("Widevine key URI is not valid base64"));
    }
    Ok(LeafInfo { key_uri, pssh, media_url, segments, prefetch })
}

/// A catalog id: digits only.
pub(crate) fn is_catalog_id(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

pub(crate) fn check_cancel(cancel: Option<&Event>, message: &str) -> Result<(), AcquireError> {
    if cancel.is_some_and(Event::is_set) {
        return Err(WebError::Cancelled(message.to_string()).into());
    }
    Ok(())
}

/// `{key: meta[key] or null}` for the keys given, in their order.
pub(crate) fn picked(meta: &Map<String, Value>, keys: &[&str]) -> Map<String, Value> {
    keys.iter().map(|key| (key.to_string(), meta.get(*key).cloned().unwrap_or(Value::Null))).collect()
}

/// A JSON whole number (not true or false, not a float).
fn whole_number(value: &Value) -> Option<i128> {
    value.as_i64().map(i128::from).or_else(|| value.as_u64().map(i128::from))
}

pub(crate) fn file_size(path: &Path) -> Result<u64, AcquireError> {
    fs::metadata(path)
        .map(|info| info.len())
        .map_err(|error| AcquireError::other(format!("media file could not be read ({})", os_error_name(&error))))
}

/// Files of an acquisition in flight: deleted when this goes out of scope, whatever happened.
pub(crate) struct Staging(pub Vec<PathBuf>);

impl Drop for Staging {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

// --- helper programs ------------------------------------------------------------------

/// What a helper program left when it ended (Python's `CompletedProcess`).
#[derive(Clone, PartialEq, Eq)]
pub struct Completed {
    /// The exit status, or minus the number of the signal that ended the program.
    pub returncode: i32,
    pub stdout: String,
    pub stderr: String,
}

impl std::fmt::Debug for Completed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Completed").field("returncode", &self.returncode).finish_non_exhaustive()
    }
}

/// Why a helper program did not run to its end.
#[derive(Debug)]
pub enum RunError {
    /// It could not be started (Python's `OSError`).
    Start(io::Error),
    /// It was still running at the time limit and has been killed.
    TimedOut,
}

fn drain(pipe: impl Read + Send + 'static) -> io::Result<JoinHandle<Vec<u8>>> {
    thread::Builder::new().name("amui-pipe".to_string()).spawn(move || {
        let mut pipe = pipe;
        let mut data = Vec::new();
        let _ = pipe.read_to_end(&mut data);
        data
    })
}

fn drained(reader: Option<JoinHandle<Vec<u8>>>) -> String {
    let data = reader.and_then(|reader| reader.join().ok()).unwrap_or_default();
    String::from_utf8_lossy(&data).into_owned()
}

/// Run a program to its end without input, its output captured, files it creates private
/// (Python's `subprocess.run(..., capture_output=True, timeout=..., umask=0o077)`).
pub(crate) fn run_captured(program: &mut Command, timeout: Duration) -> Result<Completed, RunError> {
    program.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    private_umask(program);
    let mut child = program.spawn().map_err(RunError::Start)?;
    let give_up = |mut child: Child| {
        let _ = child.kill();
        let _ = child.wait();
    };
    let stdout = child.stdout.take().map(drain).transpose();
    let stderr = child.stderr.take().map(drain).transpose();
    let (stdout, stderr) = match (stdout, stderr) {
        (Ok(stdout), Ok(stderr)) => (stdout, stderr),
        (Err(error), _) | (_, Err(error)) => {
            give_up(child);
            return Err(RunError::Start(error));
        }
    };
    match wait_timeout(&mut child, timeout) {
        Ok(Some(status)) => {
            Ok(Completed { returncode: returncode(status), stdout: drained(stdout), stderr: drained(stderr) })
        }
        Ok(None) => {
            give_up(child);
            Err(RunError::TimedOut)
        }
        Err(error) => {
            give_up(child);
            Err(RunError::Start(error))
        }
    }
}

/// The Widevine helper process: JSON lines over stdin/stdout, stderr to a private log file.
struct Helper {
    process: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<Vec<u8>>,
    log_path: PathBuf,
}

impl Helper {
    fn start(binary: &Path, log_path: &Path) -> Result<Helper, AcquireError> {
        let failed = |error: io::Error| {
            AcquireError::other(format!("could not start the DRM helper ({})", os_error_name(&error)))
        };
        let log =
            OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(log_path).map_err(failed)?;
        let mut program = Command::new(binary);
        program.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(log);
        private_umask(&mut program);
        let mut process = program.spawn().map_err(failed)?;
        let (stdin, stdout) = (process.stdin.take(), process.stdout.take());
        let (sender, lines) = mpsc::channel();
        // from here on the helper is stopped when this function fails
        let helper = Helper { process, stdin, lines, log_path: log_path.to_path_buf() };
        let reader = thread::Builder::new().name("amui-drm-helper".to_string()).spawn(move || {
            let Some(stdout) = stdout else { return };
            let mut answers = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                let read = answers.read_until(b'\n', &mut line).unwrap_or(0);
                if read == 0 || sender.send(line).is_err() {
                    break;
                }
            }
        });
        reader.map_err(failed)?;
        Ok(helper)
    }

    fn send(&mut self, message: &Value) -> Result<(), AcquireError> {
        let mut line = message.to_string();
        line.push('\n');
        let sent = self
            .stdin
            .as_mut()
            .is_some_and(|stdin| stdin.write_all(line.as_bytes()).and_then(|_| stdin.flush()).is_ok());
        if sent { Ok(()) } else { Err(AcquireError::other(format!("DRM helper closed its input{}", self.reason()))) }
    }

    fn receive(&mut self, timeout: f64) -> Result<Map<String, Value>, AcquireError> {
        let line = match self.lines.recv_timeout(seconds(timeout)) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => return Err(AcquireError::other("DRM helper timed out")),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(AcquireError::other(format!("DRM helper exited{}", self.reason())));
            }
        };
        match serde_json::from_slice(&line) {
            Ok(Value::Object(message)) => Ok(message),
            Ok(_) => Err(AcquireError::other("DRM helper sent an unexpected message")),
            Err(_) => Err(AcquireError::other("DRM helper sent an invalid message")),
        }
    }

    /// ": <last stderr line>" (fixed English messages from the helper's source; never key
    /// material).
    fn reason(&self) -> String {
        let tail = last_line(&self.log_path, 300);
        if tail.is_empty() { String::new() } else { format!(": {tail}") }
    }

    /// Close the helper's input and wait for it to end: its exit status, `None` when it is still
    /// running after `timeout` seconds.
    fn finish(&mut self, timeout: f64) -> Option<i32> {
        self.stdin = None;
        wait_timeout(&mut self.process, seconds(timeout)).ok().flatten().map(returncode)
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        terminate(&mut self.process, Duration::from_secs(5));
    }
}

fn helper_protocol(hello: &Map<String, Value>) -> i128 {
    hello.get("protocol").and_then(whole_number).unwrap_or(1)
}

// --- the acquisition --------------------------------------------------------------

/// The ALAC route as `acquire()` sees it; [`crate::lossless::Lossless`] is the real one.
pub trait LosslessRoute: Send + Sync {
    /// The rendition up to 48 kHz, cache key `<id>.lossless`.
    fn acquire_lossless(
        &self,
        catalog_id: &str,
        media_cache: &MediaCache,
        progress: Option<ProgressFn<'_>>,
        cancel: Option<&Event>,
    ) -> Result<AcquireResult, AcquireError>;

    /// The rendition above 48 kHz, cache key `<id>.hires`.
    fn acquire_hires(
        &self,
        catalog_id: &str,
        media_cache: &MediaCache,
        progress: Option<ProgressFn<'_>>,
        cancel: Option<&Event>,
    ) -> Result<AcquireResult, AcquireError>;
}

type Probe = Box<dyn Fn(&Path) -> ProbeInfo + Send + Sync>;

/// Song acquisition: the configuration, the HTTP layer, the lossless route and the answers of
/// `has_atmos()`.
pub struct Acquirer {
    config: Arc<Config>,
    http: Arc<dyn Http>,
    lossless: Arc<dyn LosslessRoute>,
    probe: Probe,
    atmos_known: Mutex<HashMap<String, bool>>,
}

impl Acquirer {
    pub fn new(config: Arc<Config>, http: Arc<dyn Http>, lossless: Arc<dyn LosslessRoute>) -> Acquirer {
        Acquirer { config, http, lossless, probe: Box::new(probe_info), atmos_known: Mutex::default() }
    }

    /// Summarize decrypted files with something else than ffprobe (tests).
    pub fn with_probe(mut self, probe: impl Fn(&Path) -> ProbeInfo + Send + Sync + 'static) -> Acquirer {
        self.probe = Box::new(probe);
        self
    }

    // --- network steps

    /// (master text, master URL) for a catalog song; credentials go to the API host only.
    fn fetch_master(&self, catalog_id: &str) -> Result<(String, String), AcquireError> {
        let params = [
            ("id", catalog_id),
            ("kind", "song"),
            ("includeLicenseUrls", "true"),
            ("hlsEncryption", "CBC"),
            ("hlsProfile", "enhancedHls"),
        ];
        let response = self.http.get_json(ASSETS_URL, &params)?;
        let asset_url = response
            .get("results")
            .and_then(Value::as_object)
            .and_then(|results| results.get("assets"))
            .and_then(Value::as_array)
            .and_then(|assets| assets.first())
            .and_then(Value::as_object)
            .and_then(|asset| asset.get("url"))
            .and_then(Value::as_str);
        let Some(asset_url) = asset_url else {
            return Err(AcquireError::other("Apple returned no usable asset for this song and account"));
        };
        let master_url = master_url_for(asset_url);
        web::validate_url(&master_url, false)?;
        Ok((self.http.get_text(&master_url)?, master_url))
    }

    /// POST the challenge to Apple's fixed license endpoint; returns the base64 license.
    fn license(&self, catalog_id: &str, key_uri: &str, challenge: &str) -> Result<String, AcquireError> {
        let body = json!({
            "challenge": challenge,
            "key-system": "com.widevine.alpha",
            "uri": key_uri,
            "adamId": catalog_id,
            "user-initiated": true,
        });
        let response = self.http.post_json(config::LICENSE_URL, &body)?;
        match response.get("license").and_then(Value::as_str).filter(|license| !license.is_empty()) {
            Some(license) if is_base64(license) => Ok(license.to_string()),
            Some(_) => Err(AcquireError::License("Apple returned a malformed license".to_string())),
            None => {
                // Only numeric status fields are safe to include, never the response body.
                let codes: Vec<String> = ["status", "errorCode"]
                    .iter()
                    .filter_map(|key| {
                        let value = response.get(*key)?;
                        (value.is_boolean() || whole_number(value).is_some()).then(|| format!("\"{key}\": {value}"))
                    })
                    .collect();
                Err(AcquireError::License(format!(
                    "Apple returned no license; status fields: {{{}}}",
                    codes.join(", ")
                )))
            }
        }
    }

    fn remember(&self, catalog_id: &str, answer: bool) {
        lock(&self.atmos_known).insert(catalog_id.to_string(), answer);
    }

    /// Manifest-only check (no license, no download): does Apple advertise an ec-3 Atmos
    /// variant?
    ///
    /// Answers are kept per catalog id for the lifetime of this `Acquirer`.
    pub fn has_atmos(&self, catalog_id: &str) -> Result<bool, AcquireError> {
        if let Some(answer) = lock(&self.atmos_known).get(catalog_id) {
            return Ok(*answer);
        }
        let (master, master_url) = self.fetch_master(catalog_id)?;
        let answer = match select_atmos(&master, &master_url) {
            Ok(_) => true,
            Err(AcquireError::NoAtmos(_)) => false,
            Err(error) => return Err(error),
        };
        self.remember(catalog_id, answer);
        Ok(answer)
    }

    /// Forget the answer for one song, or all of them.
    pub fn forget_atmos(&self, catalog_id: Option<&str>) {
        let mut known = lock(&self.atmos_known);
        match catalog_id {
            Some(catalog_id) => {
                known.remove(catalog_id);
            }
            None => known.clear(),
        }
    }

    /// Return the cached or freshly acquired decrypted file for one variant.
    ///
    /// Variant "atmos": `AUDIO="audio-atmos-*"` with codec ec-3 (highest bandwidth); none ->
    /// [`AcquireError::NoAtmos`]. Variant "stereo": `AUDIO="audio-stereo-256"` (AAC-LC), the
    /// Dolby-Atmos-off path; the same license flow with that playlist's own PSSH. Cache keys
    /// are `<id>.atmos` / `<id>.stereo`. Variant "lossless": the ALAC rendition through the
    /// local FairPlay wrapper ([`crate::lossless`]; cache key `<id>.lossless`;
    /// [`AcquireError::Lossless`] when the route cannot deliver it). Variant "hires": the same
    /// for the rendition above 48 kHz (cache key `<id>.hires`).
    /// Phases reported: "manifest", "license", "download" (with fraction), "decrypt", "done".
    pub fn acquire(
        &self,
        catalog_id: &str,
        media_cache: &MediaCache,
        variant: &str,
        mut progress: Option<ProgressFn<'_>>,
        cancel: Option<&Event>,
    ) -> Result<AcquireResult, AcquireError> {
        if !is_catalog_id(catalog_id) {
            return Err(AcquireError::other("catalog id must be numeric"));
        }
        match variant {
            "lossless" => return self.lossless.acquire_lossless(catalog_id, media_cache, progress, cancel),
            "hires" => return self.lossless.acquire_hires(catalog_id, media_cache, progress, cancel),
            _ => {}
        }
        if !matches!(variant, "atmos" | "stereo") {
            return Err(AcquireError::other(format!("unknown variant '{variant}'")));
        }
        let key = format!("{catalog_id}.{variant}");
        let mut report = |phase: &str, fraction: Option<f64>| {
            if let Some(callback) = progress.as_mut() {
                callback(phase, fraction);
            }
        };
        let cancelled = || check_cancel(cancel, "Acquisition cancelled");

        if let Some(cached) = media_cache.get(&key)? {
            let meta = media_cache.meta(&key)?.unwrap_or_default();
            report("done", Some(1.0));
            return Ok(AcquireResult {
                catalog_id: catalog_id.to_string(),
                bytes: file_size(&cached)?,
                path: cached.to_string_lossy().into_owned(),
                variant: picked(&meta, &["audio_group", "codecs", "bandwidth"]),
                duration_s: meta.get("duration_s").and_then(Value::as_f64),
                from_cache: true,
            });
        }

        if !self.config.drm_helper.is_file() {
            return Err(AcquireError::other("DRM helper binary missing (build the workspace's amdl-linux harness)"));
        }

        cancelled()?;
        report("manifest", None);
        let (master, master_url) = self.fetch_master(catalog_id)?;
        let selected = match variant {
            "atmos" => select_atmos(&master, &master_url),
            _ => select_stereo(&master, &master_url),
        };
        drop(master);
        let selected = match selected {
            Ok(selected) => selected,
            Err(error) => {
                if error.is_no_atmos() {
                    self.remember(catalog_id, false);
                }
                return Err(error);
            }
        };
        if variant == "atmos" {
            self.remember(catalog_id, true);
        }
        cancelled()?;
        let leaf = parse_leaf(&self.http.get_text(&selected.url)?, &selected.url)?;
        cancelled()?;

        let encrypted = media_cache.staging_path(&key, ".encrypted.m4a")?;
        let output = media_cache.staging_path(&key, ".decrypted.m4a")?;
        let log_path = media_cache.staging_path(&key, ".helper.log")?;
        // goes after the helper below: the helper is stopped before its files are deleted
        let _staging = Staging(vec![encrypted.clone(), log_path.clone(), output.clone()]);

        report("license", None);
        let mut helper = Helper::start(&self.config.drm_helper, &log_path)?;
        helper.send(&json!({
            "pssh": leaf.pssh,
            "encrypted_file": encrypted.to_string_lossy(),
            "output_file": output.to_string_lossy(),
            "prefetch_key": leaf.prefetch,
        }))?;
        let hello = helper.receive(HELPER_TIMEOUT_S)?;
        let challenge = hello.get("challenge").and_then(Value::as_str).filter(|challenge| !challenge.is_empty());
        let Some(challenge) = challenge else {
            return Err(AcquireError::License(format!("DRM helper produced no license challenge{}", helper.reason())));
        };
        if !leaf.prefetch && helper_protocol(&hello) < i128::from(HELPER_PROTOCOL_SINGLE_KEY) {
            return Err(AcquireError::other(HELPER_OUT_OF_DATE));
        }
        cancelled()?;
        let license = self.license(catalog_id, &leaf.key_uri, challenge)?;
        helper.send(&json!({ "license": license }))?;
        drop(license);
        if helper.receive(HELPER_TIMEOUT_S)?.get("license_accepted") != Some(&Value::Bool(true)) {
            return Err(AcquireError::License(format!(
                "DRM helper could not use the returned license{}",
                helper.reason()
            )));
        }
        cancelled()?;

        report("download", Some(0.0));
        let mut on_download = |done: u64, total: Option<u64>| {
            report("download", total.filter(|total| *total > 0).map(|total| done as f64 / total as f64));
        };
        let (size, _digest) = self.http.download(
            &leaf.media_url,
            &encrypted,
            config::MEDIA_MAX_BYTES,
            Some(&mut on_download),
            cancel,
            DOWNLOAD_TIMEOUT_S,
        )?;
        report("download", Some(1.0));
        cancelled()?;

        report("decrypt", None);
        helper.send(&json!({"decrypt": true}))?;
        let finished = helper.receive(DECRYPT_TIMEOUT_S)?;
        let code = helper.finish(5.0);
        if finished.get("decrypted") != Some(&Value::Bool(true)) || code != Some(0) {
            return Err(AcquireError::other(format!("Local media decryption failed{}", helper.reason())));
        }
        if !output.is_file() {
            return Err(AcquireError::other("DRM helper reported success but wrote no output"));
        }
        let output_bytes = file_size(&output)?;
        let reported = finished.get("output_bytes").and_then(whole_number);
        if reported.is_some_and(|reported| reported != i128::from(output_bytes)) {
            return Err(AcquireError::other("Decrypted output size does not match the helper's report"));
        }
        if output_bytes == 0 {
            return Err(AcquireError::other("Decrypted output is empty"));
        }

        let info = (self.probe)(&output);
        let mut meta = Map::new();
        meta.insert("catalog_id".to_string(), json!(catalog_id));
        meta.insert("variant".to_string(), json!(variant));
        meta.extend(selected.to_map());
        meta.insert("segments".to_string(), json!(leaf.segments));
        meta.insert("encrypted_bytes".to_string(), json!(size));
        meta.insert("bytes".to_string(), json!(output_bytes));
        meta.insert("acquired_at".to_string(), json!(util::unix_now()));
        meta.extend(info.to_map());
        let dest = media_cache.put(&key, &output, &meta)?;
        report("done", Some(1.0));
        Ok(AcquireResult {
            catalog_id: catalog_id.to_string(),
            path: dest.to_string_lossy().into_owned(),
            bytes: output_bytes,
            variant: selected.to_map(),
            duration_s: info.duration_s,
            from_cache: false,
        })
    }
}

// --- ffprobe / ffmpeg -------------------------------------------------------------

/// What ffprobe says about the first audio stream of a file; `None` for what it does not say.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProbeInfo {
    pub duration_s: Option<f64>,
    pub codec: Option<String>,
    pub profile: Option<String>,
    /// The stream's bit rate, else the container's.
    pub bit_rate: Option<i64>,
    pub sample_rate: Option<i64>,
    pub channels: Option<i64>,
    pub container_bit_rate: Option<i64>,
}

impl ProbeInfo {
    /// The summary as the media cache's meta carries it.
    pub fn to_map(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("duration_s".to_string(), json!(self.duration_s));
        map.insert("codec".to_string(), json!(self.codec));
        map.insert("profile".to_string(), json!(self.profile));
        map.insert("bit_rate".to_string(), json!(self.bit_rate));
        map.insert("sample_rate".to_string(), json!(self.sample_rate));
        map.insert("channels".to_string(), json!(self.channels));
        map.insert("container_bit_rate".to_string(), json!(self.container_bit_rate));
        map
    }
}

/// A value of ffprobe's JSON as a number; ffprobe writes most numbers as text, "N/A" for none.
fn probed_float(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::String(text) if text.is_empty() || text == "N/A" => None,
        Value::String(text) => text.trim().parse().ok(),
        other => other.as_f64(),
    }
}

fn probed_whole(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::String(text) if text.is_empty() || text == "N/A" => None,
        Value::String(text) => text.trim().parse().ok(),
        other => other.as_i64().or_else(|| other.as_f64().map(|float| float.trunc() as i64)),
    }
}

fn probe_from_json(data: &Value) -> ProbeInfo {
    let nothing = Map::new();
    let format = data.get("format").and_then(Value::as_object).unwrap_or(&nothing);
    let stream = data
        .get("streams")
        .and_then(Value::as_array)
        .and_then(|streams| streams.first())
        .and_then(Value::as_object)
        .unwrap_or(&nothing);
    let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_owned);
    // the container's duration when it names one, even an unusable one ("N/A")
    let format_duration = format.get("duration").filter(|value| match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(text) => !text.is_empty(),
        Value::Number(number) => number.as_f64() != Some(0.0),
        _ => true,
    });
    let container_bit_rate = probed_whole(format.get("bit_rate"));
    ProbeInfo {
        duration_s: match format_duration {
            Some(duration) => probed_float(Some(duration)),
            None => probed_float(stream.get("duration")),
        },
        codec: text(stream.get("codec_name")),
        profile: text(stream.get("profile")),
        bit_rate: probed_whole(stream.get("bit_rate")).filter(|rate| *rate != 0).or(container_bit_rate),
        sample_rate: probed_whole(stream.get("sample_rate")),
        channels: probed_whole(stream.get("channels")),
        container_bit_rate,
    }
}

/// ffprobe summary: duration_s, codec, profile, bit_rate (stream, else container), sample_rate,
/// channels.
///
/// Missing values are `None`; a failed ffprobe gives the empty summary (never an error).
pub fn probe_info(path: &Path) -> ProbeInfo {
    let mut ffprobe = Command::new("ffprobe");
    ffprobe
        .args(["-v", "error", "-select_streams", "a:0", "-show_entries"])
        .arg("format=duration,bit_rate:stream=codec_name,profile,sample_rate,channels,bit_rate,duration")
        .args(["-of", "json"])
        .arg(path);
    match run_captured(&mut ffprobe, seconds(FFPROBE_TIMEOUT_S)) {
        Ok(done) if done.returncode == 0 => {
            serde_json::from_str(&done.stdout).map(|data| probe_from_json(&data)).unwrap_or_default()
        }
        _ => ProbeInfo::default(),
    }
}

/// Duration in seconds via ffprobe, `None` when unavailable.
pub fn probe_duration(path: &Path) -> Option<f64> {
    probe_info(path).duration_s
}

/// ffmpeg: AAC or ALAC .m4a -> interleaved f32le 48 kHz 2-channel file. Returns frames written.
///
/// A source at another sample rate (44.1 kHz CD masters, Hi-Res ALAC) is resampled with soxr at
/// its highest precision; 48 kHz sources pass straight through. Writes `meta.json` next to
/// `out_pcm`: `{"channels": 2, "frames", "sample_rate": 48000, "source": "aac"|"alac",
/// "source_sample_rate", "resampled", "codec", "profile", "bit_rate", "duration_s",
/// "source_duration_s"}`.
pub fn decode_stereo(m4a: &Path, out_pcm: &Path) -> Result<u64, AcquireError> {
    if let Some(folder) = out_pcm.parent() {
        util::create_private_dirs(folder).map_err(|error| {
            AcquireError::other(format!("could not create the decode folder ({})", os_error_name(&error)))
        })?;
    }
    if out_pcm.exists() {
        return Err(AcquireError::other("decode target already exists; claim a fresh PCM cache directory"));
    }
    let info = probe_info(m4a);
    let resample = info.sample_rate.is_some_and(|rate| rate != 0 && rate != i64::from(config::SAMPLE_RATE));
    let mut ffmpeg = Command::new("ffmpeg");
    ffmpeg.args(["-nostdin", "-loglevel", "error", "-i"]).arg(m4a).arg("-vn");
    if resample {
        ffmpeg.arg("-af").arg(format!("aresample=resampler=soxr:precision=33:osr={}", config::SAMPLE_RATE));
    }
    ffmpeg.args(["-f", "f32le", "-acodec", "pcm_f32le", "-ac", "2", "-ar"]).arg(config::SAMPLE_RATE.to_string());
    ffmpeg.arg(out_pcm);
    let completed = match run_captured(&mut ffmpeg, seconds(FFMPEG_TIMEOUT_S)) {
        Ok(completed) => completed,
        Err(RunError::Start(error)) => {
            return Err(AcquireError::other(format!("could not run ffmpeg ({})", os_error_name(&error))));
        }
        Err(RunError::TimedOut) => {
            let _ = fs::remove_file(out_pcm);
            return Err(AcquireError::other("ffmpeg decode timed out"));
        }
    };
    if completed.returncode != 0 {
        let _ = fs::remove_file(out_pcm);
        let tail = splitlines(&completed.stderr).into_iter().rev().find(|line| !line.trim().is_empty());
        let detail = tail.map(|line| format!(": {}", line.chars().take(200).collect::<String>())).unwrap_or_default();
        return Err(AcquireError::other(format!("ffmpeg decode failed{detail}")));
    }
    let size = fs::metadata(out_pcm).map(|info| info.len()).unwrap_or(0);
    if size == 0 || !size.is_multiple_of(8) {
        let _ = fs::remove_file(out_pcm);
        return Err(AcquireError::other("ffmpeg produced no complete stereo frames"));
    }
    let frames = size / 8;
    let meta = json!({
        "channels": 2,
        "frames": frames,
        "sample_rate": config::SAMPLE_RATE,
        "source": if info.codec.as_deref() == Some("alac") { "alac" } else { "aac" },
        "source_sample_rate": info.sample_rate,
        "resampled": resample,
        "codec": info.codec,
        "profile": info.profile,
        "bit_rate": info.bit_rate,
        "duration_s": frames as f64 / f64::from(config::SAMPLE_RATE),
        "source_duration_s": info.duration_s,
    });
    util::write_private_json(&out_pcm.with_file_name("meta.json"), &meta)
        .map_err(|error| AcquireError::other(format!("could not write meta.json ({})", os_error_name(&error))))?;
    Ok(frames)
}

#[cfg(test)]
pub(crate) mod testing {
    //! Stand-ins for tests of this crate: an HTTP layer, helper programs, tone files.
    use super::*;
    use crate::web::{Call, Progress, Source};
    use std::os::unix::fs::PermissionsExt;

    /// One request as the stand-in HTTP layer saw it.
    #[derive(Debug, Clone)]
    pub struct Asked {
        pub method: String,
        pub url: String,
        pub auth: bool,
        pub body: Option<Vec<u8>>,
    }

    type Answer = Box<dyn Fn(&Asked) -> Result<Vec<u8>, WebError> + Send + Sync>;
    type Download = Box<dyn Fn(&str, &Path) -> Result<(u64, String), WebError> + Send + Sync>;

    /// An HTTP layer that answers from closures and remembers what it was asked.
    pub struct FakeHttp {
        answer: Answer,
        download: Download,
        pub asked: Mutex<Vec<Asked>>,
        pub downloads: Mutex<Vec<String>>,
    }

    impl FakeHttp {
        pub fn new(answer: impl Fn(&Asked) -> Result<Vec<u8>, WebError> + Send + Sync + 'static) -> FakeHttp {
            FakeHttp {
                answer: Box::new(answer),
                download: Box::new(|_, _| Err(WebError::other("no download expected"))),
                asked: Mutex::default(),
                downloads: Mutex::default(),
            }
        }

        /// A network nothing may use: every request fails and is counted.
        pub fn unused() -> FakeHttp {
            FakeHttp::new(|_| Err(WebError::other("network used")))
        }

        pub fn with_download(
            mut self,
            download: impl Fn(&str, &Path) -> Result<(u64, String), WebError> + Send + Sync + 'static,
        ) -> FakeHttp {
            self.download = Box::new(download);
            self
        }
    }

    impl Http for FakeHttp {
        fn request(&self, call: &Call) -> Result<Vec<u8>, WebError> {
            let asked = Asked {
                method: call.method.to_string(),
                url: call.url.to_string(),
                auth: call.auth,
                body: call.body.map(<[u8]>::to_vec),
            };
            self.asked.lock().unwrap().push(asked.clone());
            (self.answer)(&asked)
        }

        fn download(
            &self,
            url: &str,
            destination: &Path,
            _max_bytes: u64,
            _progress: Option<Progress>,
            _cancel: Option<&Event>,
            _timeout: f64,
        ) -> Result<(u64, String), WebError> {
            self.downloads.lock().unwrap().push(url.to_string());
            (self.download)(url, destination)
        }

        fn token_sources(&self) -> Vec<Source> {
            Vec::new()
        }
    }

    /// A lossless route that only notes which songs it was asked for.
    #[derive(Default)]
    pub struct FakeRoute {
        pub seen: Mutex<Vec<String>>,
    }

    impl LosslessRoute for FakeRoute {
        fn acquire_lossless(
            &self,
            catalog_id: &str,
            _media_cache: &MediaCache,
            _progress: Option<ProgressFn<'_>>,
            _cancel: Option<&Event>,
        ) -> Result<AcquireResult, AcquireError> {
            self.seen.lock().unwrap().push(catalog_id.to_string());
            Ok(AcquireResult {
                catalog_id: "dispatched".to_string(),
                path: String::new(),
                bytes: 0,
                variant: Map::new(),
                duration_s: None,
                from_cache: false,
            })
        }

        fn acquire_hires(
            &self,
            catalog_id: &str,
            _media_cache: &MediaCache,
            _progress: Option<ProgressFn<'_>>,
            _cancel: Option<&Event>,
        ) -> Result<AcquireResult, AcquireError> {
            self.seen.lock().unwrap().push(format!("{catalog_id} hires"));
            Ok(AcquireResult {
                catalog_id: "dispatched hires".to_string(),
                path: String::new(),
                bytes: 0,
                variant: Map::new(),
                duration_s: None,
                from_cache: false,
            })
        }
    }

    /// A scratch am-ui folder whose configuration `adjust` may change before it is shared. The
    /// lossless service it names is on a port nothing listens on.
    pub fn scratch(adjust: impl FnOnce(&mut Config)) -> (tempfile::TempDir, Arc<Config>) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("am-ui");
        fs::create_dir_all(root.join("workspace")).unwrap();
        let env = HashMap::from([("LOSSLESS_PORT".to_string(), "9".to_string())]);
        let mut config = Config::new(root, &dir.path().join("home"), &env);
        adjust(&mut config);
        config.ensure_dirs().unwrap();
        (dir, Arc::new(config))
    }

    /// Write a program that a test starts by its path. Another process writes the file, so no
    /// thread of this one can hand an open copy of it to a program it starts meanwhile (the
    /// kernel refuses to run a file that is open for writing).
    pub fn write_program(path: &Path, text: &str) {
        let mut writer = Command::new("tee").arg(path).stdin(Stdio::piped()).stdout(Stdio::null()).spawn().unwrap();
        writer.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
        assert!(writer.wait().unwrap().success());
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Half a second of a 440 Hz tone encoded by ffmpeg; false when this machine cannot do that
    /// (no ffmpeg or ffprobe, or no such encoder).
    pub fn tone(path: &Path, codec: &str, sample_rate: u32) -> bool {
        let found = |program: &str| {
            let mut probe = Command::new(program);
            probe.arg("-version");
            run_captured(&mut probe, Duration::from_secs(20)).is_ok_and(|done| done.returncode == 0)
        };
        if !found("ffmpeg") || !found("ffprobe") {
            return false;
        }
        let mut ffmpeg = Command::new("ffmpeg");
        ffmpeg.args(["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i"]);
        ffmpeg.arg(format!("sine=frequency=440:sample_rate={sample_rate}"));
        ffmpeg.args(["-t", "0.5", "-ac", "2", "-c:a", codec]);
        if codec == "alac" {
            ffmpeg.args(["-sample_fmt", "s32p"]);
        }
        ffmpeg.arg(path);
        run_captured(&mut ffmpeg, Duration::from_secs(60)).is_ok_and(|done| done.returncode == 0)
    }

    /// The names of the files in a folder, sorted.
    pub fn files_in(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .map(|items| items.flatten().filter(|item| item.path().is_file()).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .map(|item| item.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    //! Parsing and selection on synthetic manifests, and `acquire()` against stand-in helpers
    //! (no network, no credentials).
    use super::testing::*;
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    const MASTER_URL: &str =
        "https://aod.itunes.apple.com/itunes-assets/HLSMusic/v4/ab/cd/P1000_default.m3u8?a=1&sig=xyz";
    const ASSET_URL: &str =
        "https://aod.itunes.apple.com/itunes-assets/HLSMusic/v4/ab/cd/P1000_hvc-abc_m.m3u8?a=1&sig=xyz";
    const LEAF_URL: &str =
        "https://aod.itunes.apple.com/itunes-assets/HLSMusic/v4/ab/cd/audio-atmos-2768/P1000_default.m3u8?a=1&sig=xyz";

    fn fake_pssh() -> String {
        STANDARD.encode(b"synthetic pssh payload for unit tests only")
    }

    fn fake_key_uri() -> String {
        format!("{DATA_URI_PREFIX}{}", fake_pssh())
    }

    fn master(variants: &[(&str, &str, i64)], header: bool) -> String {
        let mut lines: Vec<String> = if header {
            vec!["#EXTM3U".into(), "#EXT-X-VERSION:7".into(), "#EXT-X-INDEPENDENT-SEGMENTS".into()]
        } else {
            vec!["#EXT-X-VERSION:7".into()]
        };
        for (group, _, _) in variants {
            lines.push(format!("#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"{group}\",AUTOSELECT=YES,NAME=\"songEnhanced\""));
        }
        for (group, codecs, bandwidth) in variants {
            lines.push(format!(
                "#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH={},BANDWIDTH={bandwidth},CODECS=\"{codecs}\",\
                 STABLE-VARIANT-ID=\"id-{group}\",AUDIO=\"{group}\"",
                bandwidth - 100
            ));
            lines.push(format!("{group}/P1000_default.m3u8?a=1&sig=xyz"));
        }
        lines.join("\n") + "\n"
    }

    fn full_master() -> String {
        master(
            &[
                ("audio-stereo-256-downmix", "mp4a.40.2", 268553),
                ("audio-stereo-256-binaural", "mp4a.40.2", 270539),
                ("audio-stereo-256", "mp4a.40.2", 270424),
                ("audio-atmos-2768", "ec-3", 772001),
                ("audio-atmos-2448", "ec-3", 451640),
                ("audio-alac-stereo-44100-24", "alac", 1739289),
                ("audio-stereo-128", "mp4a.40.2", 136812),
                ("audio-HE-stereo-64", "mp4a.40.5", 77523),
            ],
            true,
        )
    }

    /// The master of a song Apple encoded only as AAC: streams without an audio group.
    fn plain(streams: &[(&str, i64)]) -> String {
        let mut lines: Vec<String> =
            vec!["#EXTM3U".into(), "#EXT-X-VERSION:7".into(), "#EXT-X-INDEPENDENT-SEGMENTS".into()];
        for (codecs, bandwidth) in streams {
            lines.push(format!(
                "#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH={},BANDWIDTH={bandwidth},CODECS=\"{codecs}\",\
                 STABLE-VARIANT-ID=\"id-{bandwidth}\"",
                bandwidth - 100
            ));
            lines.push(format!("P1000_A2000_MS_audio_gr{}_AAC.m3u8", bandwidth / 1000));
        }
        lines.join("\n") + "\n"
    }

    fn plain_master() -> String {
        plain(&[("mp4a.40.2", 302212), ("mp4a.40.2", 156065), ("mp4a.40.5", 80139)])
    }

    fn leaf(key_uris: &[&str], media: &[&str], map_uri: &str, header: bool) -> String {
        let key_line = |uri: &str| {
            format!(
                "#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"{uri}\",KEYFORMAT=\"{WIDEVINE_KEYFORMAT}\",KEYFORMATVERSIONS=\"1\""
            )
        };
        let mut lines: Vec<String> = Vec::new();
        if header {
            lines.extend(
                ["#EXTM3U", "#EXT-X-TARGETDURATION:15", "#EXT-X-VERSION:7", "#EXT-X-PLAYLIST-TYPE:VOD"]
                    .map(String::from),
            );
        }
        lines.push(
            "#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://itunes.apple.com/P000/s1/e1\",\
             KEYFORMAT=\"com.apple.streamingkeydelivery\",KEYFORMATVERSIONS=\"1\""
                .to_string(),
        );
        lines.extend(key_uris.iter().map(|uri| key_line(uri)));
        lines.push(format!("#EXT-X-MAP:URI=\"{map_uri}\",BYTERANGE=\"1058@0\""));
        let mut offset = 1058;
        for (index, name) in media.iter().enumerate() {
            if index == 1 {
                // Apple repeats the key tags before the second segment
                lines.extend(key_uris.iter().map(|uri| key_line(uri)));
            }
            lines.push("#EXTINF:14.97600,\t".to_string());
            lines.push(format!("#EXT-X-BYTERANGE:1441601@{offset}"));
            lines.push(name.to_string());
            offset += 1441601;
        }
        lines.push("#EXT-X-ENDLIST".to_string());
        lines.join("\n") + "\n"
    }

    /// A leaf with the given keys and three segments of one media file.
    fn leaf_with_keys(key_uris: &[&str]) -> String {
        leaf(key_uris, &["media.mp4"; 3], "media.mp4", true)
    }

    fn two_key_leaf() -> String {
        leaf_with_keys(&[PREFETCH_KEY_URI, &fake_key_uri()])
    }

    fn song_key_leaf() -> String {
        leaf_with_keys(&[&fake_key_uri()])
    }

    fn is_plain_error(result: &Result<impl Sized, AcquireError>) -> bool {
        matches!(result, Err(AcquireError::Other(_)))
    }

    // --- master URL

    #[test]
    fn default_rewrite_keeps_signed_query() {
        let url = "https://aod.itunes.apple.com/itunes-assets/HLSMusic/v4/ab/cd/P1000_hvc-abc_m.m3u8?a=1&sig=xyz&e=2";
        assert_eq!(
            master_url_for(url),
            "https://aod.itunes.apple.com/itunes-assets/HLSMusic/v4/ab/cd/P1000_default.m3u8?a=1&sig=xyz&e=2"
        );
    }

    #[test]
    fn rewrite_only_touches_the_playlist_name() {
        let url = "https://aod.itunes.apple.com/P77_x/P1000_a.b.m3u8";
        assert_eq!(master_url_for(url), "https://aod.itunes.apple.com/P77_x/P1000_default.m3u8");
        let unchanged = "https://aod.itunes.apple.com/x/master.m3u8?s=1";
        assert_eq!(master_url_for(unchanged), unchanged);
    }

    // --- selection

    #[test]
    fn atmos_picks_highest_bandwidth() {
        let chosen = select_atmos(&full_master(), MASTER_URL).unwrap();
        assert_eq!(chosen.audio_group, "audio-atmos-2768");
        assert_eq!(chosen.codecs, "ec-3");
        assert_eq!(chosen.bandwidth.as_deref(), Some("772001"));
        assert_eq!(
            chosen.url,
            "https://aod.itunes.apple.com/itunes-assets/HLSMusic/v4/ab/cd/audio-atmos-2768/P1000_default.m3u8?a=1&sig=xyz"
        );
    }

    #[test]
    fn atmos_strict() {
        let stereo_only =
            master(&[("audio-stereo-256", "mp4a.40.2", 270424), ("audio-stereo-128", "mp4a.40.2", 136812)], true);
        assert!(matches!(select_atmos(&stereo_only, MASTER_URL), Err(AcquireError::NoAtmos(_))));
        // an atmos group name without the ec-3 codec is not Atmos either
        let fake = master(&[("audio-atmos-2768", "mp4a.40.2", 772001)], true);
        assert!(matches!(select_atmos(&fake, MASTER_URL), Err(AcquireError::NoAtmos(_))));
        assert!(is_plain_error(&select_atmos("<html>not a playlist</html>", MASTER_URL)));
    }

    #[test]
    fn stereo_prefers_exact_256_group() {
        let chosen = select_stereo(&full_master(), MASTER_URL).unwrap();
        assert_eq!(chosen.audio_group, "audio-stereo-256");
        assert_eq!(chosen.codecs, "mp4a.40.2");
        assert_eq!(chosen.bandwidth.as_deref(), Some("270424"));
        assert!(chosen.url.starts_with("https://aod.itunes.apple.com/"));
    }

    #[test]
    fn stereo_fallbacks() {
        let no_256 = master(
            &[
                ("audio-stereo-256-downmix", "mp4a.40.2", 268553),
                ("audio-stereo-128", "mp4a.40.2", 136812),
                ("audio-alac-stereo-44100-24", "alac", 1739289),
            ],
            true,
        );
        assert_eq!(select_stereo(&no_256, MASTER_URL).unwrap().audio_group, "audio-stereo-128");
        let suffixed_only = master(
            &[("audio-stereo-256-downmix", "mp4a.40.2", 268553), ("audio-stereo-256-binaural", "mp4a.40.2", 270539)],
            true,
        );
        assert_eq!(select_stereo(&suffixed_only, MASTER_URL).unwrap().audio_group, "audio-stereo-256-binaural");
        let alac_only = master(&[("audio-alac-stereo-44100-24", "alac", 1739289)], true);
        assert!(is_plain_error(&select_stereo(&alac_only, MASTER_URL)));
        let headless = master(&[("audio-stereo-256", "mp4a.40.2", 1)], false);
        assert!(is_plain_error(&select_stereo(&headless, MASTER_URL)));
    }

    #[test]
    fn stereo_from_a_master_without_audio_groups() {
        let chosen =
            select_stereo(&plain(&[("mp4a.40.5", 80139), ("mp4a.40.2", 302212), ("mp4a.40.2", 156065)]), MASTER_URL)
                .unwrap();
        assert_eq!(
            (chosen.audio_group.as_str(), chosen.codecs.as_str(), chosen.bandwidth.as_deref()),
            ("", "mp4a.40.2", Some("302212"))
        );
        assert!(chosen.url.starts_with("https://aod.itunes.apple.com/itunes-assets/HLSMusic/v4/ab/cd/P1000_A2000"));
        // AAC-LC before a HE-AAC stream of any bandwidth; HE-AAC when it is all there is
        let chosen = select_stereo(&plain(&[("mp4a.40.5", 400000), ("mp4a.40.2", 156065)]), MASTER_URL).unwrap();
        assert_eq!((chosen.codecs.as_str(), chosen.bandwidth.as_deref()), ("mp4a.40.2", Some("156065")));
        assert_eq!(select_stereo(&plain(&[("mp4a.40.5", 80139)]), MASTER_URL).unwrap().codecs, "mp4a.40.5");
        // a named stereo group still wins; a stream with any other codec is not taken
        let streams = plain(&[("mp4a.40.2", 999999)]);
        let mixed = full_master() + streams.splitn(4, '\n').nth(3).unwrap();
        assert_eq!(select_stereo(&mixed, MASTER_URL).unwrap().audio_group, "audio-stereo-256");
        for codecs in ["avc1.64001f,mp4a.40.2", "ec-3", "alac", ""] {
            assert!(is_plain_error(&select_stereo(&plain(&[(codecs, 302212)]), MASTER_URL)), "{codecs}");
        }
        assert!(matches!(select_atmos(&plain(&[("mp4a.40.2", 302212)]), MASTER_URL), Err(AcquireError::NoAtmos(_))));
    }

    #[test]
    fn variant_url_must_be_an_apple_host() {
        let text = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,CODECS=\"mp4a.40.2\",AUDIO=\"audio-stereo-256\"\n\
                    https://evil.example/x.m3u8\n";
        assert!(matches!(select_stereo(text, MASTER_URL), Err(AcquireError::Web(_))));
        let text = text.replace("mp4a.40.2", "ec-3").replace("audio-stereo-256", "audio-atmos-2768");
        assert!(is_plain_error(&select_atmos(&text, MASTER_URL)));
    }

    // --- leaf

    #[test]
    fn single_key_single_media() {
        let info = parse_leaf(&two_key_leaf(), LEAF_URL).unwrap();
        assert_eq!(info.key_uri, fake_key_uri());
        assert_eq!(info.pssh, fake_pssh());
        assert_eq!(
            info.media_url,
            "https://aod.itunes.apple.com/itunes-assets/HLSMusic/v4/ab/cd/audio-atmos-2768/media.mp4"
        );
        assert_eq!(info.segments, 3);
        assert!(info.prefetch);
    }

    #[test]
    fn playlist_with_the_song_key_alone() {
        let info = parse_leaf(&song_key_leaf(), LEAF_URL).unwrap();
        assert_eq!((info.key_uri, info.pssh, info.segments), (fake_key_uri(), fake_pssh(), 3));
        assert!(!info.prefetch);
    }

    #[test]
    fn prefetch_key_is_excluded_and_required_real_key() {
        assert!(is_plain_error(&parse_leaf(&leaf_with_keys(&[PREFETCH_KEY_URI]), LEAF_URL)));
    }

    #[test]
    fn two_distinct_keys_rejected() {
        let other = format!("{DATA_URI_PREFIX}{}", STANDARD.encode(b"another synthetic payload"));
        let keys = [PREFETCH_KEY_URI, &fake_key_uri(), &other];
        assert!(is_plain_error(&parse_leaf(&leaf_with_keys(&keys), LEAF_URL)));
    }

    #[test]
    fn non_data_key_rejected() {
        assert!(is_plain_error(&parse_leaf(&leaf_with_keys(&["https://aod.itunes.apple.com/key"]), LEAF_URL)));
    }

    #[test]
    fn invalid_base64_rejected() {
        let key = format!("{DATA_URI_PREFIX}@@@not base64@@@");
        assert!(is_plain_error(&parse_leaf(&leaf_with_keys(&[&key]), LEAF_URL)));
    }

    #[test]
    fn one_media_file_required() {
        let keys = [PREFETCH_KEY_URI, &fake_key_uri()];
        assert!(is_plain_error(&parse_leaf(&leaf(&keys, &["a.mp4", "b.mp4"], "a.mp4", true), LEAF_URL)));
        assert!(is_plain_error(&parse_leaf(&leaf(&keys, &["a.mp4", "a.mp4"], "init.mp4", true), LEAF_URL)));
    }

    #[test]
    fn media_url_resolution_and_host_check() {
        let keys = [PREFETCH_KEY_URI, &fake_key_uri()];
        let absolute = "https://aod.itunes.apple.com/other/path/media.mp4";
        let info = parse_leaf(&leaf(&keys, &[absolute], absolute, true), LEAF_URL).unwrap();
        assert_eq!(info.media_url, absolute);
        let evil = "https://evil.example/media.mp4";
        assert!(matches!(parse_leaf(&leaf(&keys, &[evil], evil, true), LEAF_URL), Err(AcquireError::Web(_))));
    }

    #[test]
    fn not_a_playlist() {
        let keys = [PREFETCH_KEY_URI, &fake_key_uri()];
        assert!(is_plain_error(&parse_leaf(&leaf(&keys, &["media.mp4"; 3], "media.mp4", false), LEAF_URL)));
    }

    // --- the acquirer against a stand-in network

    fn assets_answer(asset_url: &str) -> Vec<u8> {
        json!({"results": {"assets": [{"url": asset_url}]}}).to_string().into_bytes()
    }

    fn acquirer(config: &Arc<Config>, http: &Arc<FakeHttp>) -> (Acquirer, Arc<FakeRoute>) {
        let route = Arc::new(FakeRoute::default());
        (Acquirer::new(config.clone(), http.clone(), route.clone()), route)
    }

    #[test]
    fn answers_are_cached_per_process() {
        let (_dir, config) = scratch(|_| {});
        // song 1 has Atmos, every other song is stereo only
        let http = Arc::new(FakeHttp::new(|asked| {
            Ok(match asked.url.strip_prefix(ASSETS_URL) {
                Some(query) => {
                    let id = query.strip_prefix("?id=").and_then(|rest| rest.split('&').next()).unwrap_or("");
                    assets_answer(&format!("https://aod.itunes.apple.com/itunes-assets/x/P{id}_y.m3u8?sig=xyz"))
                }
                None if asked.url.contains("/P1_default.m3u8") => full_master().into_bytes(),
                None => master(&[("audio-stereo-256", "mp4a.40.2", 1)], true).into_bytes(),
            })
        }));
        let (acquirer, _route) = acquirer(&config, &http);
        let calls = || {
            let asked = http.asked.lock().unwrap();
            let asset_requests = asked.iter().filter(|asked| asked.url.starts_with(ASSETS_URL));
            asset_requests
                .map(|asked| asked.url[ASSETS_URL.len() + 4..].split('&').next().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert!(acquirer.has_atmos("1").unwrap());
        assert!(acquirer.has_atmos("1").unwrap());
        assert!(!acquirer.has_atmos("2").unwrap());
        assert!(!acquirer.has_atmos("2").unwrap());
        assert_eq!(calls(), ["1", "2"]);
        acquirer.forget_atmos(Some("1"));
        assert!(acquirer.has_atmos("1").unwrap());
        assert_eq!(calls(), ["1", "2", "1"]);
        acquirer.forget_atmos(None);
        assert!(!acquirer.has_atmos("2").unwrap());
        assert_eq!(calls(), ["1", "2", "1", "2"]);
        // the account's credentials go with the API request only; the playlist is fetched without
        let asked = http.asked.lock().unwrap();
        assert_eq!(
            asked[0].url,
            format!("{ASSETS_URL}?id=1&kind=song&includeLicenseUrls=true&hlsEncryption=CBC&hlsProfile=enhancedHls")
        );
        assert!(asked[0].auth && !asked[1].auth);
        assert_eq!(asked[1].url, "https://aod.itunes.apple.com/itunes-assets/x/P1_default.m3u8?sig=xyz");
    }

    #[test]
    fn cache_hit_needs_no_network() {
        let (_dir, config) = scratch(|_| {});
        let root = config.tmp_dir.join("test-acquire");
        fs::create_dir_all(&root).unwrap();
        let cache = MediaCache::in_directory(config.clone(), root.join("media"), config::MEDIA_CACHE_BYTES);
        let source = root.join("song.m4a");
        fs::write(&source, vec![0u8; 100]).unwrap();
        let meta =
            json!({"duration_s": 208.88, "audio_group": "audio-atmos-2768", "codecs": "ec-3", "bandwidth": "772001"});
        cache.put("1234567890.atmos", &source, meta.as_object().unwrap()).unwrap();
        let http = Arc::new(FakeHttp::unused());
        let (acquirer, _route) = acquirer(&config, &http);
        let mut phases = Vec::new();
        let mut progress = |phase: &str, fraction: Option<f64>| phases.push((phase.to_string(), fraction));
        let result = acquirer.acquire("1234567890", &cache, "atmos", Some(&mut progress), None).unwrap();
        assert!(http.asked.lock().unwrap().is_empty(), "network used");
        assert!(result.from_cache);
        assert_eq!(result.bytes, 100);
        assert_eq!(result.duration_s, Some(208.88));
        let variant = json!({"audio_group": "audio-atmos-2768", "codecs": "ec-3", "bandwidth": "772001"});
        assert_eq!(Value::Object(result.variant.clone()), variant);
        assert_eq!(result.path, root.join("media/1234567890.atmos.m4a").to_str().unwrap());
        assert_eq!(phases, [("done".to_string(), Some(1.0))]);
        assert_eq!(result.to_json()["catalog_id"], "1234567890");
    }

    #[test]
    fn cancel_before_network() {
        let helper = tempfile::NamedTempFile::new().unwrap();
        let (_dir, config) = scratch(|config| config.drm_helper = helper.path().to_path_buf());
        let cache = MediaCache::in_directory(config.clone(), config.tmp_dir.join("media"), config::MEDIA_CACHE_BYTES);
        let cancel = Event::new();
        cancel.set();
        let http = Arc::new(FakeHttp::unused());
        let (acquirer, _route) = acquirer(&config, &http);
        let error = acquirer.acquire("1", &cache, "atmos", None, Some(&cancel)).unwrap_err();
        assert!(matches!(error, AcquireError::Web(WebError::Cancelled(_))) && error.is_cancelled());
        assert!(http.asked.lock().unwrap().is_empty(), "network used");
    }

    #[test]
    fn rejects_bad_ids_and_variants() {
        let (_dir, config) = scratch(|_| {});
        let cache = MediaCache::in_directory(config.clone(), config.tmp_dir.join("media"), config::MEDIA_CACHE_BYTES);
        let http = Arc::new(FakeHttp::unused());
        let (acquirer, route) = acquirer(&config, &http);
        assert!(is_plain_error(&acquirer.acquire("not-an-id", &cache, "atmos", None, None)));
        let error = acquirer.acquire("1", &cache, "flac", None, None).unwrap_err();
        assert_eq!(error, AcquireError::other("unknown variant 'flac'"));
        assert!(is_plain_error(&acquirer.acquire("not-an-id", &cache, "lossless", None, None)));
        assert!(route.seen.lock().unwrap().is_empty() && http.asked.lock().unwrap().is_empty());
    }

    #[test]
    fn lossless_variant_is_dispatched_to_the_lossless_module() {
        let (_dir, config) = scratch(|_| {});
        let cache = MediaCache::in_directory(config.clone(), config.tmp_dir.join("media"), config::MEDIA_CACHE_BYTES);
        let http = Arc::new(FakeHttp::unused());
        let (acquirer, route) = acquirer(&config, &http);
        assert_eq!(acquirer.acquire("1", &cache, "lossless", None, None).unwrap().catalog_id, "dispatched");
        assert_eq!(acquirer.acquire("2", &cache, "hires", None, None).unwrap().catalog_id, "dispatched hires");
        assert_eq!(*route.seen.lock().unwrap(), ["1", "2 hires"]);
    }

    // --- acquire() against a stand-in helper: what it is told about the playlist's keys, and
    // that a helper built before it could decrypt a single-key file is refused before a license
    // is asked for

    const FAKE_HELPER: &str = r#"#!/usr/bin/env python3
import json, sys
start = json.loads(sys.stdin.readline())
with open(RECORD, "w") as record:
    json.dump(start, record)
hello = {"challenge": "c3ludGhldGljIGNoYWxsZW5nZQ=="}
if PROTOCOL is not None:
    hello["protocol"] = PROTOCOL
print(json.dumps(hello), flush=True)
if not sys.stdin.readline():
    sys.exit(0)
print(json.dumps({"license_accepted": True}), flush=True)
if not sys.stdin.readline():
    sys.exit(0)
with open(start["output_file"], "wb") as output:
    output.write(b"decrypted")
print(json.dumps({"decrypted": True, "output_bytes": 9}), flush=True)
"#;

    /// What one `acquire()` against the stand-ins left behind.
    struct Outcome {
        result: Result<AcquireResult, AcquireError>,
        /// The first message the helper got.
        first: Option<Value>,
        license_requests: Vec<Value>,
        downloads: Vec<String>,
        phases: Vec<(String, Option<f64>)>,
        cache: MediaCache,
        acquirer: Acquirer,
        http: Arc<FakeHttp>,
    }

    struct Conversation {
        _dir: tempfile::TempDir,
        config: Arc<Config>,
        root: PathBuf,
        /// What Apple's license endpoint answers.
        license_answer: Value,
    }

    impl Conversation {
        fn new() -> Conversation {
            let (dir, config) = scratch(|config| config.drm_helper = config.root.join("helper"));
            let root = config.tmp_dir.join("test-helper");
            fs::create_dir_all(&root).unwrap();
            Conversation { _dir: dir, config, root, license_answer: json!({"license": "bGljZW5zZQ=="}) }
        }

        /// `protocol` is the Python literal the stand-in answers with (`None`: no "protocol").
        fn run(&self, master_text: &str, leaf_text: &str, protocol: &str, variant: &str) -> Outcome {
            let record = self.root.join("first-message.json");
            let _ = fs::remove_file(&record);
            let helper =
                FAKE_HELPER.replace("RECORD", &format!("{:?}", record.to_str().unwrap())).replace("PROTOCOL", protocol);
            write_program(&self.config.drm_helper, &helper);
            self.run_with_helper(master_text, leaf_text, variant)
        }

        fn run_with_helper(&self, master_text: &str, leaf_text: &str, variant: &str) -> Outcome {
            let (master_text, leaf_text) = (master_text.to_string(), leaf_text.to_string());
            let license_answer = self.license_answer.to_string();
            let http = FakeHttp::new(move |asked| {
                Ok(if asked.url.starts_with(ASSETS_URL) {
                    assets_answer(ASSET_URL)
                } else if asked.url == MASTER_URL {
                    master_text.clone().into_bytes()
                } else if asked.url == config::LICENSE_URL {
                    license_answer.clone().into_bytes()
                } else {
                    leaf_text.clone().into_bytes()
                })
            })
            .with_download(|_url, destination| {
                fs::write(destination, b"encrypted bytes").unwrap();
                Ok((15, "digest".to_string()))
            });
            let http = Arc::new(http);
            let cache =
                MediaCache::in_directory(self.config.clone(), self.root.join("media"), config::MEDIA_CACHE_BYTES);
            let acquirer = Acquirer::new(self.config.clone(), http.clone(), Arc::new(FakeRoute::default()))
                .with_probe(|_| ProbeInfo { duration_s: Some(12.0), ..ProbeInfo::default() });
            let mut phases = Vec::new();
            let mut progress = |phase: &str, fraction: Option<f64>| phases.push((phase.to_string(), fraction));
            let result = acquirer.acquire("1", &cache, variant, Some(&mut progress), None);
            let license_requests = http
                .asked
                .lock()
                .unwrap()
                .iter()
                .filter(|asked| asked.url == config::LICENSE_URL)
                .map(|asked| {
                    assert!(asked.auth && asked.method == "POST");
                    serde_json::from_slice(asked.body.as_deref().unwrap()).unwrap()
                })
                .collect();
            let downloads = http.downloads.lock().unwrap().clone();
            let first = util::read_json(&self.root.join("first-message.json"));
            Outcome { result, first, license_requests, downloads, phases, cache, acquirer, http }
        }

        /// Files a finished or failed acquisition left in the staging folder.
        fn staged(&self) -> Vec<String> {
            files_in(&self.config.tmp_dir)
        }
    }

    #[test]
    fn two_key_playlist_works_with_an_older_helper() {
        let conversation = Conversation::new();
        let outcome = conversation.run(&full_master(), &two_key_leaf(), "None", "stereo");
        let result = outcome.result.unwrap();
        let first = outcome.first.unwrap();
        assert_eq!(first["prefetch_key"], true);
        assert_eq!(first["pssh"], json!(fake_pssh()));
        assert_eq!(outcome.license_requests.len(), 1);
        assert_eq!(result.variant["audio_group"], "audio-stereo-256");
        assert_eq!(fs::read(&result.path).unwrap(), b"decrypted");
        assert!(outcome.cache.get("1.stereo").unwrap().is_some());
        // what goes to Apple's license endpoint, and that nothing is left in the staging folder
        let request = &outcome.license_requests[0];
        assert_eq!(request["challenge"], "c3ludGhldGljIGNoYWxsZW5nZQ==");
        assert_eq!((&request["key-system"], &request["adamId"]), (&json!("com.widevine.alpha"), &json!("1")));
        assert_eq!((&request["uri"], &request["user-initiated"]), (&json!(fake_key_uri()), &json!(true)));
        assert_eq!(
            outcome.downloads,
            ["https://aod.itunes.apple.com/itunes-assets/HLSMusic/v4/ab/cd/audio-stereo-256/media.mp4"]
        );
        assert_eq!(conversation.staged(), Vec::<String>::new());
        assert_eq!((result.bytes, result.duration_s, result.from_cache), (9, Some(12.0), false));
    }

    #[test]
    fn single_key_playlist_tells_the_helper() {
        let conversation = Conversation::new();
        let outcome = conversation.run(&plain_master(), &song_key_leaf(), "2", "stereo");
        let result = outcome.result.unwrap();
        assert_eq!(outcome.first.unwrap()["prefetch_key"], false);
        assert_eq!(outcome.license_requests.len(), 1);
        let variant = json!({"audio_group": "", "codecs": "mp4a.40.2", "bandwidth": "302212"});
        assert_eq!(Value::Object(result.variant), variant);
    }

    #[test]
    fn single_key_playlist_is_refused_by_an_older_helper_before_any_license() {
        let conversation = Conversation::new();
        for protocol in ["None", "1", "True", "'2'"] {
            let outcome = conversation.run(&plain_master(), &song_key_leaf(), protocol, "stereo");
            let error = outcome.result.unwrap_err();
            assert!(matches!(error, AcquireError::Other(_)), "{protocol}: {error:?}"); // not a license failure
            assert!(error.to_string().contains("./setup.sh"), "{protocol}");
            assert!(outcome.license_requests.is_empty(), "{protocol}");
            assert!(outcome.downloads.is_empty(), "{protocol}");
            assert_eq!(outcome.cache.get("1.stereo").unwrap(), None, "{protocol}");
            assert_eq!(files_in(&conversation.root.join("media")), Vec::<String>::new(), "{protocol}");
            assert_eq!(conversation.staged(), Vec::<String>::new(), "{protocol}");
        }
    }

    // --- beyond the Python tests

    #[test]
    fn an_acquirer_can_be_shared_between_threads() {
        fn shared<T: Send + Sync + 'static>() {}
        shared::<Acquirer>();
    }

    #[test]
    fn an_atmos_acquisition_records_what_it_found() {
        let conversation = Conversation::new();
        let outcome = conversation.run(&full_master(), &two_key_leaf(), "2", "atmos");
        let result = outcome.result.unwrap();
        let variant = json!({"audio_group": "audio-atmos-2768", "codecs": "ec-3", "bandwidth": "772001"});
        assert_eq!(Value::Object(result.variant.clone()), variant);
        let phases: Vec<(&str, Option<f64>)> = outcome.phases.iter().map(|(phase, at)| (phase.as_str(), *at)).collect();
        let expected = [
            ("manifest", None),
            ("license", None),
            ("download", Some(0.0)),
            ("download", Some(1.0)),
            ("decrypt", None),
            ("done", Some(1.0)),
        ];
        assert_eq!(phases, expected);
        let meta = outcome.cache.meta("1.atmos").unwrap().unwrap();
        let mut keys: Vec<&str> = meta.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "acquired_at",
                "audio_group",
                "bandwidth",
                "bit_rate",
                "bytes",
                "catalog_id",
                "channels",
                "codec",
                "codecs",
                "container_bit_rate",
                "duration_s",
                "encrypted_bytes",
                "profile",
                "sample_rate",
                "segments",
                "variant"
            ]
        );
        assert_eq!(
            (&meta["catalog_id"], &meta["variant"], &meta["segments"]),
            (&json!("1"), &json!("atmos"), &json!(3))
        );
        assert_eq!(
            (&meta["encrypted_bytes"], &meta["bytes"], &meta["duration_s"]),
            (&json!(15), &json!(9), &json!(12.0))
        );
        // the answer is known without another request, and the second acquisition is a cache hit
        let requests = outcome.http.asked.lock().unwrap().len();
        assert!(outcome.acquirer.has_atmos("1").unwrap());
        let again = outcome.acquirer.acquire("1", &outcome.cache, "atmos", None, None).unwrap();
        assert!(again.from_cache && again.variant == result.variant && again.duration_s == Some(12.0));
        assert_eq!(outcome.http.asked.lock().unwrap().len(), requests);

        // a song without Atmos: the strict answer, remembered as well
        let stereo_only = master(&[("audio-stereo-256", "mp4a.40.2", 270424)], true);
        let outcome = Conversation::new().run(&stereo_only, &two_key_leaf(), "2", "atmos");
        assert!(outcome.result.unwrap_err().is_no_atmos());
        let requests = outcome.http.asked.lock().unwrap().len();
        assert!(!outcome.acquirer.has_atmos("1").unwrap());
        assert_eq!(outcome.http.asked.lock().unwrap().len(), requests);
        assert!(outcome.first.is_none() && outcome.license_requests.is_empty());
    }

    #[test]
    fn license_failures_name_status_fields_only() {
        let mut conversation = Conversation::new();
        conversation.license_answer =
            json!({"status": -1002, "errorCode": 5, "message": "body text", "customerMessage": 7.5});
        let outcome = conversation.run(&full_master(), &two_key_leaf(), "2", "stereo");
        assert_eq!(
            outcome.result.unwrap_err(),
            AcquireError::License(
                "Apple returned no license; status fields: {\"status\": -1002, \"errorCode\": 5}".into()
            )
        );
        assert!(outcome.downloads.is_empty());
        conversation.license_answer = json!({"license": ""});
        let outcome = conversation.run(&full_master(), &two_key_leaf(), "2", "stereo");
        let error = outcome.result.unwrap_err();
        assert_eq!(error, AcquireError::License("Apple returned no license; status fields: {}".into()));
        conversation.license_answer = json!({"license": "not base64!"});
        let outcome = conversation.run(&full_master(), &two_key_leaf(), "2", "stereo");
        assert_eq!(outcome.result.unwrap_err(), AcquireError::License("Apple returned a malformed license".into()));
        assert_eq!(conversation.staged(), Vec::<String>::new());
    }

    #[test]
    fn a_failing_helper_is_named_by_its_last_line() {
        let conversation = Conversation::new();
        let run = |script: &str| {
            write_program(&conversation.config.drm_helper, script);
            let outcome = conversation.run_with_helper(&full_master(), &two_key_leaf(), "stereo");
            assert_eq!(conversation.staged(), Vec::<String>::new());
            (outcome.result.unwrap_err(), outcome.license_requests.len(), outcome.downloads.len())
        };
        // the helper cannot make a challenge
        let (error, licenses, downloads) = run(
            "#!/bin/sh\nread -r start\necho 'starting' >&2\necho 'Bundled CDM/device initialization failed' >&2\nexit 1\n",
        );
        assert_eq!(error, AcquireError::other("DRM helper exited: Bundled CDM/device initialization failed"));
        assert_eq!((licenses, downloads), (0, 0));
        // it answers with something that is no challenge
        let (error, licenses, _) = run("#!/bin/sh\nread -r start\necho '{\"challenge\": \"\"}'\nread -r next\n");
        assert_eq!(error, AcquireError::License("DRM helper produced no license challenge".into()));
        assert_eq!(licenses, 0);
        let (error, _, _) = run("#!/bin/sh\nread -r start\necho 'not json'\nread -r next\n");
        assert_eq!(error, AcquireError::other("DRM helper sent an invalid message"));
        let (error, _, _) = run("#!/bin/sh\nread -r start\necho '[1]'\nread -r next\n");
        assert_eq!(error, AcquireError::other("DRM helper sent an unexpected message"));
        // it refuses the license
        let refuses = "#!/bin/sh\nread -r start\necho '{\"challenge\": \"c3ludGhldGlj\", \"protocol\": 2}'\nread -r license\n\
                       echo 'License contains no usable content key' >&2\necho '{\"license_accepted\": false}'\nread -r next\n";
        let (error, licenses, downloads) = run(refuses);
        let expected = "DRM helper could not use the returned license: License contains no usable content key";
        assert_eq!(error, AcquireError::License(expected.into()));
        assert_eq!((licenses, downloads), (1, 0));
        // it fails to decrypt, or reports a size the file does not have
        let accepts = "#!/bin/sh\nread -r start\necho '{\"challenge\": \"c3ludGhldGlj\", \"protocol\": 2}'\nread -r license\n\
                       echo '{\"license_accepted\": true}'\nread -r decrypt\n";
        let (error, licenses, downloads) = run(&format!("{accepts}echo 'MP4 decryption failed' >&2\nexit 1\n"));
        assert_eq!(error, AcquireError::other("DRM helper exited: MP4 decryption failed"));
        assert_eq!((licenses, downloads), (1, 1));
        let (error, _, _) = run(&format!("{accepts}echo '{{\"decrypted\": true, \"output_bytes\": 9}}'\n"));
        assert_eq!(error, AcquireError::other("DRM helper reported success but wrote no output"));
        let (error, _, _) = run(&format!("{accepts}echo '{{\"decrypted\": true}}'\nexit 3\n"));
        assert_eq!(error, AcquireError::other("Local media decryption failed"));
        // no helper at all
        fs::remove_file(&conversation.config.drm_helper).unwrap();
        let outcome = conversation.run_with_helper(&full_master(), &two_key_leaf(), "stereo");
        let missing = "DRM helper binary missing (build the workspace's amdl-linux harness)";
        assert_eq!(outcome.result.unwrap_err(), AcquireError::other(missing));
        assert!(outcome.http.asked.lock().unwrap().is_empty());
    }

    #[test]
    fn playlist_tags_and_urls_are_read_as_python_reads_them() {
        let attrs =
            attributes("AVERAGE-BANDWIDTH=99,BANDWIDTH=100,CODECS=\"mp4a.40.2,ec-3\",AUDIO=\"a,b\",X=,BANDWIDTH=7");
        assert_eq!(attrs["CODECS"], "mp4a.40.2,ec-3");
        assert_eq!((attrs["AUDIO"].as_str(), attrs["X"].as_str(), attrs["BANDWIDTH"].as_str()), ("a,b", "", "7"));
        assert_eq!(attrs["AVERAGE-BANDWIDTH"], "99");
        assert_eq!(ASSETS_URL, format!("https://{}/v1/play/assets", config::API_HOST));
        assert_eq!(path_span("https://h/a/b.m3u8?x=1#f"), (9, 18));
        assert_eq!(path_span("https://h"), (9, 9));
        assert_eq!(
            master_url_for("https://h/P1_a.m3u8/P2_b.m3u8?u=P3_c.m3u8"),
            "https://h/P1_default.m3u8/P2_default.m3u8?u=P3_c.m3u8"
        );
        // the first of equal bandwidths wins; a missing or odd bandwidth counts as 0
        let ties = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=5,CODECS=\"ec-3\",AUDIO=\"audio-atmos-1\"\r\nfirst.m3u8\r\n\
                    #EXT-X-STREAM-INF:BANDWIDTH=5,CODECS=\"ec-3\",AUDIO=\"audio-atmos-2\"\r\nsecond.m3u8\r\n\
                    #EXT-X-STREAM-INF:CODECS=\"ec-3\",AUDIO=\"audio-atmos-3\"\r\nthird.m3u8\r\n";
        assert_eq!(select_atmos(ties, MASTER_URL).unwrap().audio_group, "audio-atmos-1");
        let missing =
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=5,CODECS=\"ec-3\",AUDIO=\"audio-atmos-1\"\n#EXT-X-ENDLIST\n";
        assert_eq!(
            select_atmos(missing, MASTER_URL).unwrap_err(),
            AcquireError::other("Atmos variant is missing its playlist URL")
        );
        assert_eq!(
            select_stereo(missing, MASTER_URL).unwrap_err(),
            AcquireError::other("A stream variant is missing its playlist URL")
        );
        // Python's b64decode(validate=True): padding required, stray characters refused
        for (text, valid) in [
            ("QUJD", true),
            ("QQ==", true),
            ("QR==", true),
            ("", true),
            ("QQ", false),
            ("QUJD\n", false),
            ("QQ==QQ==", false),
        ] {
            assert_eq!(is_base64(text), valid, "{text:?}");
        }
        // nothing a log could show names a URL or a key
        let chosen = select_atmos(&full_master(), MASTER_URL).unwrap();
        let shown = format!("{chosen:?} {:?}", parse_leaf(&two_key_leaf(), LEAF_URL).unwrap());
        assert!(!shown.contains("https://") && !shown.contains("sig=") && !shown.contains(&fake_pssh()), "{shown}");
    }

    #[test]
    fn the_ffprobe_summary() {
        let answer = json!({
            "streams": [{"codec_name": "aac", "profile": "LC", "sample_rate": "44100", "channels": 2,
                         "bit_rate": "257168", "duration": "208.926000"}],
            "format": {"duration": "208.926440", "bit_rate": "258567"},
        });
        let info = probe_from_json(&answer);
        assert_eq!(info.duration_s, Some(208.92644));
        assert_eq!((info.codec.as_deref(), info.profile.as_deref()), (Some("aac"), Some("LC")));
        assert_eq!((info.bit_rate, info.container_bit_rate), (Some(257168), Some(258567)));
        assert_eq!((info.sample_rate, info.channels), (Some(44100), Some(2)));
        let keys: Vec<String> = info.to_map().keys().cloned().collect();
        assert_eq!(
            keys,
            ["duration_s", "codec", "profile", "bit_rate", "sample_rate", "channels", "container_bit_rate"]
        );
        // ALAC in a fragmented file: no stream bit rate, the container's stands in
        let answer = json!({
            "streams": [{"codec_name": "alac", "sample_rate": "48000", "channels": 2, "bit_rate": "N/A"}],
            "format": {"duration": "N/A", "bit_rate": "1995449"},
        });
        let info = probe_from_json(&answer);
        assert_eq!((info.duration_s, info.profile.clone(), info.bit_rate), (None, None, Some(1995449)));
        let answer = json!({"streams": [{"duration": "12.5", "bit_rate": "0"}], "format": {}});
        assert_eq!(probe_from_json(&answer), ProbeInfo { duration_s: Some(12.5), ..ProbeInfo::default() });
        for nothing in [json!({}), json!({"streams": [], "format": null}), json!([1]), json!({"streams": ["x"]})] {
            assert_eq!(probe_from_json(&nothing), ProbeInfo::default());
        }
        // a file that is no media file, and one that is not there
        let (_dir, config) = scratch(|_| {});
        let junk = config.tmp_dir.join("junk.m4a");
        fs::write(&junk, b"decrypted").unwrap();
        assert_eq!(probe_info(&junk), ProbeInfo::default());
        assert_eq!(probe_duration(&config.tmp_dir.join("absent.m4a")), None);
    }

    #[test]
    fn decode_stereo_resamples_other_rates() {
        let (_dir, config) = scratch(|_| {});
        let source = config.tmp_dir.join("tone-44k.m4a");
        if !tone(&source, "aac", 44100) {
            eprintln!("skipped: ffmpeg/ffprobe needed");
            return;
        }
        let out = config.tmp_dir.join("pcm/1.stereo/stereo.f32le");
        let frames = decode_stereo(&source, &out).unwrap();
        assert_eq!(frames, fs::metadata(&out).unwrap().len() / 8);
        assert!((frames as f64 / 48000.0 - 0.5).abs() < 0.06, "{frames}");
        let meta = util::read_json_object(&out.with_file_name("meta.json")).unwrap();
        assert_eq!(
            (&meta["source"], &meta["source_sample_rate"], &meta["resampled"]),
            (&json!("aac"), &json!(44100), &json!(true))
        );
        assert_eq!(
            (&meta["channels"], &meta["frames"], &meta["sample_rate"]),
            (&json!(2), &json!(frames), &json!(48000))
        );
        assert_eq!(meta["duration_s"], json!(frames as f64 / 48000.0));
        use std::os::unix::fs::PermissionsExt;
        for file in [&out, &out.with_file_name("meta.json")] {
            assert_eq!(fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o600, "{file:?}");
        }
        assert_eq!(crate::render::PcmFile::new(&out, 2).unwrap().meta["codec"], "aac");
        let again = decode_stereo(&source, &out).unwrap_err();
        assert_eq!(again, AcquireError::other("decode target already exists; claim a fresh PCM cache directory"));
        // something ffmpeg cannot read: its last line is the reason, and nothing is left behind
        let junk = config.tmp_dir.join("junk.m4a");
        fs::write(&junk, b"decrypted").unwrap();
        let failed = config.tmp_dir.join("pcm/2.stereo/stereo.f32le");
        let error = decode_stereo(&junk, &failed).unwrap_err().to_string();
        assert!(error.starts_with("ffmpeg decode failed: "), "{error}");
        assert!(!failed.exists() && !failed.with_file_name("meta.json").exists());
    }
}
