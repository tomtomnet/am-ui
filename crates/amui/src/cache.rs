//! Bounded private caches: decrypted media, PCM renders, artwork.
//!
//! All files are 0600 in 0700 directories under the data folder (everything below `var/` is
//! git-ignored). Each cache keeps an `index.json` next to its files with sizes, last-access times
//! and (for media) the ffprobe metadata; eviction is LRU. Every public method takes the instance
//! lock, so one cache object can be shared by the player's worker threads and the UI thread.
//!
//! Keys are `<catalog id>.<variant>` ("1234567890.atmos", "1234567890.stereo",
//! "1234567890.lossless", "1234567890.hires", "1234567890.binaural"): plain file-name-safe
//! strings, never paths. "lossless" holds ALAC up to 48 kHz and "hires" ALAC above it, so that the
//! two versions of a song never take each other's place.
//!
//! Beyond the stub contract (documented in research/engine-pipeline.md): `MediaCache::entries()`,
//! `total_bytes()`, `remove()`; `PcmCache::path()`, `kind()`, `entries()`, `remove()`,
//! `split_key()`; `ArtCache::entries()`, `total_bytes()`, `evict()`.
//!
//! Deliberate differences from `amui/cache.py`:
//!
//! * The folders come from a [`Config`]: `MediaCache::new(config)` is Python's `MediaCache()`,
//!   and `in_directory()` names another folder or bound (tests, tools).
//! * Every method that goes to the file system returns a `Result`: [`CacheError::Key`] is
//!   Python's `ValueError`, [`CacheError::Busy`] its `CacheBusyError`, [`CacheError::Io`] an
//!   `OSError` (the text names no path).
//! * `entries()` gives typed records ([`MediaEntry`], [`PcmEntry`], [`ArtEntry`]) sorted by key.
//! * Index and meta files are written by `util::write_private_json`: the same layout (sorted
//!   keys, one space of indentation, final newline); a float may be spelled differently and
//!   non-ASCII text is written as it is instead of `\u` escapes. Both engines read both.
//! * Entries with the very same last-access time are evicted in key order (Python: in the order
//!   its dictionary happened to hold them).
//! * An index with fields of unexpected types is read as far as it makes sense, and a file that
//!   cannot be deleted during eviction is left behind; Python raised in both cases. The same
//!   goes for a PCM entry whose folder vanishes while the index is read.
//! * `PcmCache::evict()` takes any collection of keys; one that is not a PCM cache key is an
//!   error only when it looks like a key (as in Python), and an indexed entry with such a name
//!   counts as a song of its own instead of stopping the eviction.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::config::{self, Config};
use crate::render;
use crate::util;

/// What a cache refuses or cannot do; the messages are safe to show and log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CacheError {
    /// The key is not a plain `<id>.<variant>` string (Python's `ValueError`).
    #[error("{0}")]
    Key(String),
    /// The entry is marked in use by another live process: its player is reading or rendering
    /// it (Python's `CacheBusyError`).
    #[error("{0}")]
    Busy(String),
    /// A file operation failed (Python's `OSError`).
    #[error("{0}")]
    Io(String),
}

impl From<io::Error> for CacheError {
    fn from(error: io::Error) -> CacheError {
        CacheError::Io(error.to_string())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `[0-9A-Za-z][0-9A-Za-z._-]{0,200}`, the whole text.
fn key_matches(key: &str) -> bool {
    let bytes = key.as_bytes();
    bytes.len() <= 201
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes[1..].iter().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn check_key(key: &str) -> Result<&str, CacheError> {
    if !key_matches(key) || key.contains("..") {
        return Err(CacheError::Key("cache key must be a plain '<id>.<variant>' string".to_string()));
    }
    Ok(key)
}

fn mkdir_private(path: &Path) -> io::Result<()> {
    util::create_private_dirs(path)?;
    util::make_private(path, 0o700);
    Ok(())
}

/// The modification time in nanoseconds, 0 when the file is not there.
fn mtime_ns(path: &Path) -> i128 {
    fs::metadata(path).map(|info| i128::from(info.mtime()) * 1_000_000_000 + i128::from(info.mtime_nsec())).unwrap_or(0)
}

fn modified_seconds(info: &fs::Metadata) -> f64 {
    info.mtime() as f64 + info.mtime_nsec() as f64 / 1e9
}

/// A rename, falling back to a copy when source and destination are on different file systems.
fn move_file(source: &Path, dest: &Path) -> io::Result<()> {
    match fs::rename(source, dest) {
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
            let name = dest.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            let temporary = dest.with_file_name(format!("{name}.moving"));
            let _ = fs::remove_file(&temporary);
            let mut output = util::create_private_new(&temporary)?;
            io::copy(&mut fs::File::open(source)?, &mut output)?;
            drop(output);
            fs::rename(&temporary, dest)?;
            let _ = fs::remove_file(source);
            Ok(())
        }
        other => other,
    }
}

/// Delete a file, a link or a whole folder; what cannot be deleted stays.
fn rmtree(path: &Path) {
    match fs::symlink_metadata(path) {
        Ok(info) if info.is_dir() => {
            let _ = fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = fs::remove_file(path);
        }
        Err(_) => {}
    }
}

/// `bytes` random bytes as hex text, for names no other call gets.
fn token_hex(bytes: usize) -> String {
    let mut random = vec![0u8; bytes];
    let filled = fs::File::open("/dev/urandom").and_then(|mut source| source.read_exact(&mut random)).is_ok();
    if !filled {
        // no random device (a bare sandbox): the hasher's per-instance keys differ on every call
        use std::hash::{BuildHasher, Hasher};
        for chunk in random.chunks_mut(8) {
            let value = std::collections::hash_map::RandomState::new().build_hasher().finish().to_le_bytes();
            chunk.copy_from_slice(&value[..chunk.len()]);
        }
    }
    random.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The "entries" object of an index file; empty when the file is missing or something else.
fn recorded_entries(index: &Path) -> Map<String, Value> {
    match util::read_json_object(index).and_then(|mut raw| raw.remove("entries")) {
        Some(Value::Object(entries)) => entries,
        _ => Map::new(),
    }
}

/// The recorded last access of one index entry, 0 when it has none.
fn recorded_last_access(entry: Option<&Value>) -> f64 {
    entry.and_then(Value::as_object).and_then(|entry| entry.get("last_access")).and_then(Value::as_f64).unwrap_or(0.0)
}

// --- decrypted media ---------------------------------------------------------------

/// One file of the media cache.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaEntry {
    pub size: u64,
    /// Seconds since the Unix epoch.
    pub last_access: f64,
    /// What `acquire` recorded: variant, codec, duration...
    pub meta: Map<String, Value>,
}

#[derive(Default)]
struct MediaState {
    entries: Option<BTreeMap<String, MediaEntry>>,
    index_mtime: i128,
}

/// Decrypted .m4a files keyed by `<catalog id>.<variant>` (variant "atmos" = E-AC-3 JOC,
/// "stereo" = AAC, "lossless" = ALAC, "hires" = ALAC above 48 kHz).
pub struct MediaCache {
    config: Arc<Config>,
    pub directory: PathBuf,
    pub max_bytes: u64,
    state: Mutex<MediaState>,
}

impl MediaCache {
    pub const INDEX: &'static str = "index.json";

    /// The engine's media cache: `config.media_cache_dir`, bounded by `MEDIA_CACHE_BYTES`.
    pub fn new(config: Arc<Config>) -> MediaCache {
        let directory = config.media_cache_dir.clone();
        MediaCache::in_directory(config, directory, config::MEDIA_CACHE_BYTES)
    }

    /// A media cache in another folder or with another bound.
    pub fn in_directory(config: Arc<Config>, directory: impl Into<PathBuf>, max_bytes: u64) -> MediaCache {
        MediaCache { config, directory: directory.into(), max_bytes, state: Mutex::default() }
    }

    // --- internals (call with the lock held)

    fn file(&self, key: &str) -> PathBuf {
        self.directory.join(format!("{key}.m4a"))
    }

    fn load<'a>(&self, state: &'a mut MediaState) -> Result<&'a mut BTreeMap<String, MediaEntry>, CacheError> {
        let index = self.directory.join(Self::INDEX);
        // re-read when another process rewrote the index (a command-line run next to the player)
        if state.entries.is_none() || mtime_ns(&index) != state.index_mtime {
            mkdir_private(&self.directory)?;
            state.index_mtime = mtime_ns(&index);
            let mut entries = BTreeMap::new();
            for (key, entry) in &recorded_entries(&index) {
                let Value::Object(fields) = entry else { continue };
                if !key_matches(key) {
                    continue;
                }
                // file gone: drop the record
                let Ok(info) = fs::metadata(self.file(key)) else { continue };
                let meta = fields.get("meta").and_then(Value::as_object).cloned().unwrap_or_default();
                let last_access = recorded_last_access(Some(entry));
                entries.insert(key.clone(), MediaEntry { size: info.len(), last_access, meta });
            }
            // files the index lost (a crash between the move and the save)
            for item in fs::read_dir(&self.directory)?.flatten() {
                let name = item.file_name();
                let Some(key) = name.to_str().and_then(|name| name.strip_suffix(".m4a")) else { continue };
                if entries.contains_key(key) || !key_matches(key) {
                    continue;
                }
                let Ok(info) = fs::metadata(item.path()) else { continue };
                if info.is_file() {
                    let adopted =
                        MediaEntry { size: info.len(), last_access: modified_seconds(&info), meta: Map::new() };
                    entries.insert(key.to_string(), adopted);
                }
            }
            state.entries = Some(entries);
        }
        Ok(state.entries.get_or_insert_with(BTreeMap::new))
    }

    fn save(&self, state: &mut MediaState) -> Result<(), CacheError> {
        let index = self.directory.join(Self::INDEX);
        let entries: Map<String, Value> = state
            .entries
            .iter()
            .flatten()
            .map(|(key, entry)| {
                let record = json!({"size": entry.size, "last_access": entry.last_access, "meta": entry.meta});
                (key.clone(), record)
            })
            .collect();
        util::write_private_json(&index, &json!({"version": 1, "entries": entries}))?;
        state.index_mtime = mtime_ns(&index);
        Ok(())
    }

    fn evict_locked(&self, state: &mut MediaState, protect: Option<&str>) -> Result<bool, CacheError> {
        let entries = self.load(state)?;
        let mut total: u64 = entries.values().map(|entry| entry.size).sum();
        let mut oldest_first: Vec<(String, f64, u64)> =
            entries.iter().map(|(key, entry)| (key.clone(), entry.last_access, entry.size)).collect();
        oldest_first.sort_by(|a, b| a.1.total_cmp(&b.1));
        let mut changed = false;
        for (key, _, size) in oldest_first {
            if total <= self.max_bytes {
                break;
            }
            if protect == Some(key.as_str()) {
                continue;
            }
            let _ = fs::remove_file(self.file(&key));
            total -= size;
            entries.remove(&key);
            changed = true;
        }
        Ok(changed)
    }

    // --- contract

    /// Existing file (touches LRU) or `None`.
    pub fn get(&self, key: &str) -> Result<Option<PathBuf>, CacheError> {
        let mut state = lock(&self.state);
        let entries = self.load(&mut state)?;
        if !entries.contains_key(key) {
            return Ok(None);
        }
        let path = self.file(key);
        let found = path.is_file();
        if found {
            if let Some(entry) = entries.get_mut(key) {
                entry.last_access = util::unix_now();
            }
        } else {
            entries.remove(key);
        }
        self.save(&mut state)?;
        Ok(found.then_some(path))
    }

    /// What was recorded with the file, `None` when the cache does not hold it.
    pub fn meta(&self, key: &str) -> Result<Option<Map<String, Value>>, CacheError> {
        let mut state = lock(&self.state);
        let entries = self.load(&mut state)?;
        Ok(entries.get(key).filter(|_| self.file(key).is_file()).map(|entry| entry.meta.clone()))
    }

    /// Move `source` into the cache (rename), record meta, evict LRU over `max_bytes`.
    pub fn put(&self, key: &str, source: &Path, meta: &Map<String, Value>) -> Result<PathBuf, CacheError> {
        check_key(key)?;
        let mut state = lock(&self.state);
        let entries = self.load(&mut state)?;
        mkdir_private(&self.directory)?;
        let dest = self.file(key);
        fs::set_permissions(source, fs::Permissions::from_mode(0o600))?;
        move_file(source, &dest)?;
        let size = fs::metadata(&dest)?.len();
        entries.insert(key.to_string(), MediaEntry { size, last_access: util::unix_now(), meta: meta.clone() });
        self.evict_locked(&mut state, Some(key))?;
        self.save(&mut state)?;
        Ok(dest)
    }

    /// Fresh private path under `config.tmp_dir` for in-flight downloads and decrypts (not
    /// created).
    pub fn staging_path(&self, key: &str, suffix: &str) -> Result<PathBuf, CacheError> {
        check_key(key)?;
        mkdir_private(&self.config.tmp_dir)?;
        Ok(self.config.tmp_dir.join(format!("{key}.{}.{}{suffix}", std::process::id(), token_hex(6))))
    }

    pub fn evict(&self) -> Result<(), CacheError> {
        let mut state = lock(&self.state);
        if self.evict_locked(&mut state, None)? {
            self.save(&mut state)?;
        }
        Ok(())
    }

    // --- extensions

    /// A snapshot of what the cache holds.
    pub fn entries(&self) -> Result<BTreeMap<String, MediaEntry>, CacheError> {
        let mut state = lock(&self.state);
        Ok(self.load(&mut state)?.clone())
    }

    pub fn total_bytes(&self) -> Result<u64, CacheError> {
        let mut state = lock(&self.state);
        Ok(self.load(&mut state)?.values().map(|entry| entry.size).sum())
    }

    pub fn remove(&self, key: &str) -> Result<bool, CacheError> {
        let mut state = lock(&self.state);
        let entries = self.load(&mut state)?;
        if key_matches(key) {
            let _ = fs::remove_file(self.file(key));
        }
        let existed = entries.remove(key).is_some();
        if existed {
            self.save(&mut state)?;
        }
        Ok(existed)
    }
}

// --- decoded audio -----------------------------------------------------------------

/// One entry of the PCM cache.
#[derive(Debug, Clone, PartialEq)]
pub struct PcmEntry {
    /// Seconds since the Unix epoch.
    pub last_access: f64,
    /// The decode is finished and its descriptor agrees with the PCM file.
    pub complete: bool,
    /// Size of the PCM file so far.
    pub bytes: u64,
}

#[derive(Default)]
struct PcmState {
    /// key -> last access
    entries: Option<BTreeMap<String, f64>>,
    index_mtime: i128,
}

/// Decoded audio per `<catalog id>.<kind>`: kind "atmos" = `<dir>/atmos-916.f32le` +
/// `render-summary.json` (+ `render.log`); kind "stereo" = `<dir>/stereo.f32le` + `meta.json`
/// (the AAC decode); kind "lossless" = `<dir>/lossless.f32le` + `meta.json` (the ALAC decode);
/// kind "hires" = `<dir>/hires.f32le` + `meta.json` (the decode of ALAC above 48 kHz, resampled
/// to the output's rate); kind "binaural" = `<dir>/binaural.f32le` + `meta.json`.
///
/// In-use markers: a player writes `<dir>/.inuse-<pid>` while it plays, prefetches or renders an
/// entry (`mark_inuse`/`unmark_inuse`). `claim()` refuses and `evict()` skips entries another
/// *live* process marked, so a command-line run or a second player next to the application can
/// never delete or re-render the directory the application's feeder is reading. Markers of dead
/// pids are stale and removed.
pub struct PcmCache {
    pub directory: PathBuf,
    pub keep: usize,
    state: Mutex<PcmState>,
}

impl PcmCache {
    pub const INDEX: &'static str = "index.json";
    pub const INUSE_PREFIX: &'static str = ".inuse-";
    /// kind -> (PCM file, descriptor file, channels)
    pub const KINDS: [(&'static str, (&'static str, &'static str, usize)); 5] = [
        ("atmos", (render::PCM_NAME, render::SUMMARY_NAME, config::CHANNELS)),
        ("stereo", ("stereo.f32le", "meta.json", 2)),
        ("lossless", ("lossless.f32le", "meta.json", 2)),
        ("hires", ("hires.f32le", "meta.json", 2)),
        ("binaural", ("binaural.f32le", "meta.json", 2)),
    ];

    /// The engine's PCM cache: `config.pcm_cache_dir`, keeping `PCM_CACHE_KEEP` songs.
    pub fn new(config: &Config) -> PcmCache {
        PcmCache::in_directory(&config.pcm_cache_dir, config::PCM_CACHE_KEEP)
    }

    /// A PCM cache in another folder or keeping another number of songs.
    pub fn in_directory(directory: impl Into<PathBuf>, keep: usize) -> PcmCache {
        PcmCache { directory: directory.into(), keep, state: Mutex::default() }
    }

    /// (PCM file, descriptor file, channels) of a kind.
    fn files_of(kind: &str) -> Option<(&'static str, &'static str, usize)> {
        Self::KINDS.iter().find(|(name, _)| *name == kind).map(|(_, files)| *files)
    }

    /// "1234567890.atmos" -> ("1234567890", "atmos"); an error for unknown kinds.
    pub fn split_key(key: &str) -> Result<(&str, &str), CacheError> {
        check_key(key)?;
        match key.rsplit_once('.') {
            Some((song, kind)) if !song.is_empty() && Self::files_of(kind).is_some() => Ok((song, kind)),
            _ => Err(CacheError::Key(
                "PCM cache key must end in one of ['atmos', 'binaural', 'hires', 'lossless', 'stereo']".to_string(),
            )),
        }
    }

    pub fn kind<'a>(&self, key: &'a str) -> Result<&'a str, CacheError> {
        Ok(Self::split_key(key)?.1)
    }

    /// The entry's directory (whether or not it exists).
    pub fn path(&self, key: &str) -> Result<PathBuf, CacheError> {
        Ok(self.directory.join(check_key(key)?))
    }

    pub fn pcm_path(&self, key: &str) -> Result<PathBuf, CacheError> {
        let (_, kind) = Self::split_key(key)?;
        let pcm_name = Self::files_of(kind).map_or("", |(pcm_name, _, _)| pcm_name);
        Ok(self.directory.join(key).join(pcm_name))
    }

    // --- internals (lock held)

    fn load<'a>(&self, state: &'a mut PcmState) -> Result<&'a mut BTreeMap<String, f64>, CacheError> {
        let index = self.directory.join(Self::INDEX);
        if state.entries.is_none() || mtime_ns(&index) != state.index_mtime {
            mkdir_private(&self.directory)?;
            state.index_mtime = mtime_ns(&index);
            let recorded = recorded_entries(&index);
            let mut entries = BTreeMap::new();
            for item in fs::read_dir(&self.directory)?.flatten() {
                let name = item.file_name();
                let Some(name) = name.to_str() else { continue };
                if name.starts_with('.') || Self::split_key(name).is_err() {
                    continue;
                }
                let Ok(info) = fs::metadata(item.path()) else { continue };
                if !info.is_dir() {
                    continue;
                }
                let recorded_at = recorded_last_access(recorded.get(name));
                let last_access = if recorded_at != 0.0 { recorded_at } else { modified_seconds(&info) };
                entries.insert(name.to_string(), last_access);
            }
            state.entries = Some(entries);
        }
        Ok(state.entries.get_or_insert_with(BTreeMap::new))
    }

    fn save(&self, state: &mut PcmState) -> Result<(), CacheError> {
        let index = self.directory.join(Self::INDEX);
        let entries: Map<String, Value> = state
            .entries
            .iter()
            .flatten()
            .map(|(key, last_access)| (key.clone(), json!({"last_access": last_access})))
            .collect();
        util::write_private_json(&index, &json!({"version": 1, "entries": entries}))?;
        state.index_mtime = mtime_ns(&index);
        Ok(())
    }

    fn touch_locked(&self, state: &mut PcmState, key: &str) -> Result<(), CacheError> {
        self.load(state)?.insert(key.to_string(), util::unix_now());
        self.save(state)
    }

    /// Whether the entry is a complete decode: its descriptor is there and agrees with the PCM
    /// file. (Python's `_validate` returned the descriptor, which nothing read.)
    fn valid(&self, key: &str) -> bool {
        let Ok((_, kind)) = Self::split_key(key) else { return false };
        let Some((pcm_name, descriptor_name, channels)) = Self::files_of(kind) else { return false };
        let directory = self.directory.join(key);
        if kind == "atmos" {
            return render::load_summary(&directory).is_ok();
        }
        let descriptor = directory.join(descriptor_name);
        // validate_pcm_file() has compared the frame and channel counts of a meta.json object
        if !descriptor.is_file() || render::validate_pcm_file(&directory.join(pcm_name), channels).is_err() {
            return false;
        }
        match util::read_json(&descriptor) {
            None | Some(Value::Object(_)) | Some(Value::Null) | Some(Value::Bool(false)) => true,
            Some(Value::Number(number)) => number.as_f64() == Some(0.0),
            Some(Value::String(text)) => text.is_empty(),
            Some(Value::Array(items)) => items.is_empty(),
            Some(Value::Bool(true)) => false,
        }
    }

    // --- contract

    /// Directory of a finished, validated decode (summary or meta present, PCM size matches) or
    /// `None`.
    pub fn complete(&self, key: &str) -> Result<Option<PathBuf>, CacheError> {
        let directory = self.path(key)?;
        let mut state = lock(&self.state);
        // anything short of valid is "not complete"
        if !directory.is_dir() || !self.valid(key) {
            return Ok(None);
        }
        self.touch_locked(&mut state, key)?;
        Ok(Some(directory))
    }

    /// Fresh empty directory for a new decode (removes a stale or partial one).
    ///
    /// [`CacheError::Busy`] when another live process holds an in-use marker on the entry.
    pub fn claim(&self, key: &str) -> Result<PathBuf, CacheError> {
        Self::split_key(key)?;
        let mut state = lock(&self.state);
        mkdir_private(&self.directory)?;
        let directory = self.directory.join(key);
        if let Some(pid) = self.live_users(&directory).first() {
            return Err(CacheError::Busy(format!("{key} is in use by process {pid}")));
        }
        if fs::symlink_metadata(&directory).is_ok() {
            rmtree(&directory);
        }
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        self.touch_locked(&mut state, key)?;
        Ok(directory)
    }

    // --- in-use markers

    /// Pids of other live processes that marked the entry; stale markers are removed on the
    /// way.
    fn live_users(&self, directory: &Path) -> Vec<libc::pid_t> {
        let mut users = Vec::new();
        let Ok(items) = fs::read_dir(directory) else { return users };
        for item in items.flatten() {
            let name = item.file_name();
            let Some(pid) = name.to_str().and_then(|name| name.strip_prefix(Self::INUSE_PREFIX)) else { continue };
            let Ok(pid) = pid.parse::<libc::pid_t>() else { continue };
            if pid <= 0 || pid.unsigned_abs() == std::process::id() {
                continue;
            }
            // SAFETY: signal 0 delivers nothing; it only asks whether the process exists.
            let gone =
                unsafe { libc::kill(pid, 0) } != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if gone {
                let _ = fs::remove_file(item.path());
            } else {
                users.push(pid); // alive (also when it belongs to someone else)
            }
        }
        users
    }

    fn marker(&self, key: &str) -> PathBuf {
        self.directory.join(key).join(format!("{}{}", Self::INUSE_PREFIX, std::process::id()))
    }

    /// Note that this process reads or writes the entry (nothing happens when the directory
    /// does not exist).
    pub fn mark_inuse(&self, key: &str) -> Result<(), CacheError> {
        check_key(key)?;
        let _state = lock(&self.state);
        if self.directory.join(key).is_dir() {
            let _ = OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(self.marker(key));
        }
        Ok(())
    }

    pub fn unmark_inuse(&self, key: &str) -> Result<(), CacheError> {
        check_key(key)?;
        let _state = lock(&self.state);
        match fs::remove_file(self.marker(key)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }

    pub fn in_use_elsewhere(&self, key: &str) -> Result<bool, CacheError> {
        check_key(key)?;
        let _state = lock(&self.state);
        Ok(!self.live_users(&self.directory.join(key)).is_empty())
    }

    pub fn touch(&self, key: &str) -> Result<(), CacheError> {
        check_key(key)?;
        let mut state = lock(&self.state);
        self.touch_locked(&mut state, key)
    }

    /// Delete least recently used entries beyond `keep` songs, never those in `protect`.
    ///
    /// Songs are counted by catalog id: every variant of a kept song stays.
    pub fn evict<I, S>(&self, protect: I) -> Result<(), CacheError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let song_of = |key: &str| Self::split_key(key).map_or(key, |(song, _)| song).to_string();
        let mut protect: HashSet<String> = protect.into_iter().map(|key| key.as_ref().to_string()).collect();
        let mut state = lock(&self.state);
        let entries = self.load(&mut state)?;
        // another process's entry
        let in_use = entries.keys().filter(|key| !self.live_users(&self.directory.join(key)).is_empty());
        protect.extend(in_use.cloned());
        let mut keep_songs: HashSet<String> = HashSet::new();
        for key in protect.iter().filter(|key| key_matches(key)) {
            keep_songs.insert(Self::split_key(key)?.0.to_string());
        }
        let mut recent_first: Vec<(&String, f64)> = entries.iter().map(|(key, at)| (key, *at)).collect();
        recent_first.sort_by(|a, b| b.1.total_cmp(&a.1));
        for (key, _) in recent_first {
            if keep_songs.len() >= self.keep {
                break; // protected songs count toward `keep`; the most recent fill the rest
            }
            keep_songs.insert(song_of(key));
        }
        let doomed: Vec<String> = entries
            .keys()
            .filter(|key| !protect.contains(*key) && !keep_songs.contains(&song_of(key)))
            .cloned()
            .collect();
        for key in &doomed {
            rmtree(&self.directory.join(key));
            entries.remove(key);
        }
        if !doomed.is_empty() {
            self.save(&mut state)?;
        }
        Ok(())
    }

    // --- extensions

    /// A snapshot of what the cache holds.
    pub fn entries(&self) -> Result<BTreeMap<String, PcmEntry>, CacheError> {
        let mut state = lock(&self.state);
        let entries = self.load(&mut state)?;
        let snapshot = entries.iter().map(|(key, last_access)| {
            let pcm = self.pcm_path(key).ok().and_then(|pcm| fs::metadata(pcm).ok());
            let entry = PcmEntry {
                last_access: *last_access,
                complete: self.valid(key),
                bytes: pcm.map_or(0, |info| info.len()),
            };
            (key.clone(), entry)
        });
        Ok(snapshot.collect())
    }

    pub fn remove(&self, key: &str) -> Result<bool, CacheError> {
        let directory = self.path(key)?;
        let mut state = lock(&self.state);
        let entries = self.load(&mut state)?;
        let on_disk = fs::metadata(&directory).is_ok();
        rmtree(&directory);
        let existed = entries.remove(key).is_some() || on_disk;
        if existed {
            self.save(&mut state)?;
        }
        Ok(existed)
    }
}

// --- artwork -----------------------------------------------------------------------

/// One image of the artwork cache.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtEntry {
    pub size: u64,
    /// Seconds since the Unix epoch.
    pub last_access: f64,
}

#[derive(Default)]
struct ArtState {
    entries: Option<BTreeMap<String, ArtEntry>>,
    /// Touches that have not reached the index yet.
    dirty: bool,
    flushed: Option<Instant>,
    index_mtime: i128,
}

/// Artwork bytes keyed by URL (sha256 file names), LRU by size.
pub struct ArtCache {
    pub directory: PathBuf,
    pub max_bytes: u64,
    state: Mutex<ArtState>,
}

impl ArtCache {
    pub const INDEX: &'static str = "index.json";
    /// Touches from `get()` reach the index at most this often (`put()` always writes).
    pub const FLUSH_INTERVAL_S: f64 = 10.0;

    /// The engine's artwork cache: `config.art_cache_dir`, bounded by `ART_CACHE_BYTES`.
    pub fn new(config: &Config) -> ArtCache {
        ArtCache::in_directory(&config.art_cache_dir, config::ART_CACHE_BYTES)
    }

    /// An artwork cache in another folder or with another bound.
    pub fn in_directory(directory: impl Into<PathBuf>, max_bytes: u64) -> ArtCache {
        ArtCache { directory: directory.into(), max_bytes, state: Mutex::default() }
    }

    /// The file name an image URL is kept under.
    pub fn name_for(url: &str) -> String {
        Sha256::digest(url.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn is_name(name: &str) -> bool {
        name.len() == 64 && name.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    }

    fn load<'a>(&self, state: &'a mut ArtState) -> Result<&'a mut BTreeMap<String, ArtEntry>, CacheError> {
        let index = self.directory.join(Self::INDEX);
        if state.entries.is_none() || !(state.dirty || mtime_ns(&index) == state.index_mtime) {
            mkdir_private(&self.directory)?;
            state.index_mtime = mtime_ns(&index);
            let recorded = recorded_entries(&index);
            let mut entries = BTreeMap::new();
            for item in fs::read_dir(&self.directory)?.flatten() {
                let name = item.file_name();
                let Some(name) = name.to_str().filter(|name| Self::is_name(name)) else { continue };
                let Ok(info) = fs::metadata(item.path()) else { continue };
                if !info.is_file() {
                    continue;
                }
                let recorded_at = recorded_last_access(recorded.get(name));
                let last_access = if recorded_at != 0.0 { recorded_at } else { modified_seconds(&info) };
                entries.insert(name.to_string(), ArtEntry { size: info.len(), last_access });
            }
            state.entries = Some(entries);
        }
        Ok(state.entries.get_or_insert_with(BTreeMap::new))
    }

    fn save(&self, state: &mut ArtState) -> Result<(), CacheError> {
        let index = self.directory.join(Self::INDEX);
        let entries: Map<String, Value> = state
            .entries
            .iter()
            .flatten()
            .map(|(name, entry)| (name.clone(), json!({"size": entry.size, "last_access": entry.last_access})))
            .collect();
        util::write_private_json(&index, &json!({"version": 1, "entries": entries}))?;
        state.dirty = false;
        state.flushed = Some(Instant::now());
        state.index_mtime = mtime_ns(&index);
        Ok(())
    }

    fn evict_locked(&self, state: &mut ArtState, protect: Option<&str>) -> Result<bool, CacheError> {
        let entries = self.load(state)?;
        let mut total: u64 = entries.values().map(|entry| entry.size).sum();
        let mut oldest_first: Vec<(String, f64, u64)> =
            entries.iter().map(|(name, entry)| (name.clone(), entry.last_access, entry.size)).collect();
        oldest_first.sort_by(|a, b| a.1.total_cmp(&b.1));
        let mut changed = false;
        for (name, _, size) in oldest_first {
            if total <= self.max_bytes {
                break;
            }
            if protect == Some(name.as_str()) {
                continue;
            }
            let _ = fs::remove_file(self.directory.join(&name));
            total -= size;
            entries.remove(&name);
            changed = true;
        }
        Ok(changed)
    }

    pub fn get(&self, url: &str) -> Result<Option<Vec<u8>>, CacheError> {
        let name = Self::name_for(url);
        let mut state = lock(&self.state);
        let entries = self.load(&mut state)?;
        if !entries.contains_key(&name) {
            return Ok(None);
        }
        let Ok(data) = fs::read(self.directory.join(&name)) else {
            entries.remove(&name);
            state.dirty = true;
            return Ok(None);
        };
        if let Some(entry) = entries.get_mut(&name) {
            entry.last_access = util::unix_now();
        }
        state.dirty = true;
        if state.flushed.is_none_or(|flushed| flushed.elapsed().as_secs_f64() > Self::FLUSH_INTERVAL_S) {
            self.save(&mut state)?;
        }
        Ok(Some(data))
    }

    pub fn put(&self, url: &str, data: &[u8]) -> Result<(), CacheError> {
        let name = Self::name_for(url);
        let mut state = lock(&self.state);
        let entries = self.load(&mut state)?;
        util::write_private_atomic(&self.directory.join(&name), data)?;
        entries.insert(name.clone(), ArtEntry { size: data.len() as u64, last_access: util::unix_now() });
        self.evict_locked(&mut state, Some(&name))?;
        self.save(&mut state)
    }

    // --- extensions

    pub fn evict(&self) -> Result<(), CacheError> {
        let mut state = lock(&self.state);
        if self.evict_locked(&mut state, None)? || state.dirty {
            self.save(&mut state)?;
        }
        Ok(())
    }

    /// Write the touches `get()` collected to the index.
    pub fn flush(&self) -> Result<(), CacheError> {
        let mut state = lock(&self.state);
        if state.dirty {
            self.save(&mut state)?;
        }
        Ok(())
    }

    /// A snapshot of what the cache holds, by file name.
    pub fn entries(&self) -> Result<BTreeMap<String, ArtEntry>, CacheError> {
        let mut state = lock(&self.state);
        Ok(self.load(&mut state)?.clone())
    }

    pub fn total_bytes(&self) -> Result<u64, CacheError> {
        let mut state = lock(&self.state);
        Ok(self.load(&mut state)?.values().map(|entry| entry.size).sum())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::testing::scratch_config;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    /// A scratch am-ui folder; `root` is a private folder for the test's own files.
    struct Scratch {
        _dir: tempfile::TempDir,
        config: Arc<Config>,
        root: PathBuf,
    }

    impl Scratch {
        fn new() -> Scratch {
            let (dir, config) = scratch_config();
            let root = config.tmp_dir.join("test-cache");
            mkdir_private(&root).unwrap();
            Scratch { _dir: dir, config, root }
        }

        fn make_file(&self, name: &str, size: usize) -> PathBuf {
            let path = self.root.join(name);
            fs::write(&path, vec![0u8; size]).unwrap();
            path
        }

        fn media(&self, max_bytes: u64) -> MediaCache {
            MediaCache::in_directory(self.config.clone(), self.root.join("media"), max_bytes)
        }

        fn pcm(&self, keep: usize) -> PcmCache {
            PcmCache::in_directory(self.root.join("pcm"), keep)
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn pause() {
        std::thread::sleep(Duration::from_millis(10));
    }

    fn names(directory: &Path, keep: impl Fn(&fs::DirEntry) -> bool) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .unwrap()
            .map(|item| item.unwrap())
            .filter(|item| keep(item))
            .map(|item| item.file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn folders(directory: &Path) -> Vec<String> {
        names(directory, |item| item.path().is_dir())
    }

    const NO_KEYS: [&str; 0] = [];

    // --- MediaCache

    #[test]
    fn put_get_meta_and_permissions() {
        let scratch = Scratch::new();
        let cache = scratch.media(10_000);
        let source = scratch.make_file("src.m4a", 1234);
        let meta = object(json!({"duration_s": 1.5, "codec": "eac3"}));
        let dest = cache.put("1.atmos", &source, &meta).unwrap();
        assert_eq!(dest, scratch.root.join("media/1.atmos.m4a"));
        assert!(!source.exists());
        assert_eq!(mode(&dest), 0o600);
        assert_eq!(mode(dest.parent().unwrap()), 0o700);
        assert_eq!(mode(&dest.with_file_name("index.json")), 0o600);
        assert_eq!(cache.get("1.atmos").unwrap(), Some(dest));
        assert_eq!(cache.meta("1.atmos").unwrap(), Some(meta));
        assert_eq!(cache.get("1.stereo").unwrap(), None);
        assert_eq!(cache.meta("1.stereo").unwrap(), None);
        assert_eq!(cache.total_bytes().unwrap(), 1234);
    }

    #[test]
    fn rejects_unsafe_keys() {
        let scratch = Scratch::new();
        let cache = scratch.media(config::MEDIA_CACHE_BYTES);
        for key in ["../x", "a/b", ".hidden", ""] {
            let source = scratch.make_file("s.m4a", 1);
            assert!(matches!(cache.put(key, &source, &Map::new()), Err(CacheError::Key(_))), "{key:?}");
            assert!(matches!(cache.staging_path(key, ".tmp"), Err(CacheError::Key(_))), "{key:?}");
        }
    }

    #[test]
    fn staging_path_is_unique_and_private() {
        let scratch = Scratch::new();
        let cache = scratch.media(config::MEDIA_CACHE_BYTES);
        let a = cache.staging_path("1.atmos", ".encrypted.m4a").unwrap();
        let b = cache.staging_path("1.atmos", ".encrypted.m4a").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.parent(), Some(scratch.config.tmp_dir.as_path()));
        let name = a.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("1.atmos.") && name.ends_with(".encrypted.m4a"), "{name}");
        assert!(!a.exists());
        assert_eq!(mode(&scratch.config.tmp_dir), 0o700);
    }

    #[test]
    fn lru_eviction_over_max_bytes() {
        let scratch = Scratch::new();
        let cache = scratch.media(2500);
        cache.put("1.atmos", &scratch.make_file("a", 1000), &Map::new()).unwrap();
        pause();
        cache.put("2.atmos", &scratch.make_file("b", 1000), &Map::new()).unwrap();
        pause();
        assert!(cache.get("1.atmos").unwrap().is_some()); // touch: 2 is now the least recently used
        pause();
        cache.put("3.atmos", &scratch.make_file("c", 1000), &Map::new()).unwrap();
        assert_eq!(cache.get("2.atmos").unwrap(), None);
        assert!(cache.get("1.atmos").unwrap().is_some());
        assert!(cache.get("3.atmos").unwrap().is_some());
        assert_eq!(cache.total_bytes().unwrap(), 2000);
        let files = names(&scratch.root.join("media"), |item| item.file_name().to_string_lossy().ends_with(".m4a"));
        assert_eq!(files, ["1.atmos.m4a", "3.atmos.m4a"]);
    }

    #[test]
    fn new_entry_is_never_evicted_even_when_oversized() {
        let scratch = Scratch::new();
        let cache = scratch.media(500);
        cache.put("1.atmos", &scratch.make_file("a", 1000), &Map::new()).unwrap();
        assert!(cache.get("1.atmos").unwrap().is_some());
        cache.put("2.atmos", &scratch.make_file("b", 1000), &Map::new()).unwrap();
        assert_eq!(cache.get("1.atmos").unwrap(), None);
        assert!(cache.get("2.atmos").unwrap().is_some());
    }

    #[test]
    fn index_reload_and_lost_file() {
        let scratch = Scratch::new();
        let directory = scratch.root.join("media");
        let meta = object(json!({"duration_s": 2}));
        scratch.media(config::MEDIA_CACHE_BYTES).put("1.atmos", &scratch.make_file("a", 10), &meta).unwrap();
        let reopened = scratch.media(config::MEDIA_CACHE_BYTES);
        assert_eq!(reopened.meta("1.atmos").unwrap(), Some(meta));
        fs::remove_file(directory.join("1.atmos.m4a")).unwrap();
        assert_eq!(reopened.get("1.atmos").unwrap(), None);
        assert_eq!(reopened.entries().unwrap(), BTreeMap::new());
        // a file the index never recorded is adopted with empty meta
        fs::write(directory.join("7.stereo.m4a"), b"xxxxx").unwrap();
        let fresh = scratch.media(config::MEDIA_CACHE_BYTES);
        assert_eq!(fresh.get("7.stereo").unwrap(), Some(directory.join("7.stereo.m4a")));
        assert_eq!(fresh.meta("7.stereo").unwrap(), Some(Map::new()));
    }

    #[test]
    fn remove() {
        let scratch = Scratch::new();
        let cache = scratch.media(config::MEDIA_CACHE_BYTES);
        cache.put("1.atmos", &scratch.make_file("a", 10), &Map::new()).unwrap();
        assert!(cache.remove("1.atmos").unwrap());
        assert!(!cache.remove("1.atmos").unwrap());
        assert_eq!(cache.get("1.atmos").unwrap(), None);
    }

    // --- PcmCache

    fn write_atmos_entry(directory: &Path, frames: usize, overrides: Value) {
        fs::create_dir_all(directory).unwrap();
        fs::write(directory.join("atmos-916.f32le"), vec![0u8; frames * 64]).unwrap();
        let mut summary = json!({
            "frames": frames, "bytes": frames * 64, "channels": 16, "sample_rate": 48000,
            "channel_fallback_blocks": 0, "nonfinite_samples": 0,
        });
        for (key, value) in overrides.as_object().unwrap() {
            summary[key] = value.clone();
        }
        fs::write(directory.join("render-summary.json"), summary.to_string()).unwrap();
    }

    fn write_stereo_entry(directory: &Path, frames: usize, name: &str, meta_frames: Option<usize>, meta: bool) {
        fs::create_dir_all(directory).unwrap();
        fs::write(directory.join(name), vec![0u8; frames * 8]).unwrap();
        if meta {
            let meta = json!({"channels": 2, "frames": meta_frames.unwrap_or(frames), "sample_rate": 48000});
            fs::write(directory.join("meta.json"), meta.to_string()).unwrap();
        }
    }

    #[test]
    fn split_key() {
        assert_eq!(PcmCache::split_key("1234567890.atmos").unwrap(), ("1234567890", "atmos"));
        assert_eq!(PcmCache::split_key("1.binaural").unwrap(), ("1", "binaural"));
        for bad in ["1", "1.flac", "a/b.atmos", ".atmos"] {
            assert!(matches!(PcmCache::split_key(bad), Err(CacheError::Key(_))), "{bad}");
        }
    }

    #[test]
    fn claim_makes_fresh_private_dir() {
        let scratch = Scratch::new();
        let cache = scratch.pcm(3);
        let first = cache.claim("1.atmos").unwrap();
        assert_eq!(mode(&first), 0o700);
        fs::write(first.join("atmos-916.f32le"), b"partial").unwrap();
        let again = cache.claim("1.atmos").unwrap();
        assert_eq!(first, again);
        assert_eq!(fs::read_dir(&again).unwrap().count(), 0);
        assert_eq!(cache.complete("1.atmos").unwrap(), None);
    }

    #[test]
    fn complete_validates_atmos() {
        let scratch = Scratch::new();
        let cache = scratch.pcm(3);
        let path = |key: &str| cache.path(key).unwrap();
        write_atmos_entry(&path("1.atmos"), 480, json!({}));
        assert_eq!(cache.complete("1.atmos").unwrap(), Some(path("1.atmos")));
        write_atmos_entry(&path("2.atmos"), 480, json!({"channel_fallback_blocks": 1}));
        assert_eq!(cache.complete("2.atmos").unwrap(), None);
        write_atmos_entry(&path("3.atmos"), 480, json!({"nonfinite_samples": 2}));
        assert_eq!(cache.complete("3.atmos").unwrap(), None);
        write_atmos_entry(&path("4.atmos"), 480, json!({}));
        fs::write(path("4.atmos").join("atmos-916.f32le"), vec![0u8; 479 * 64]).unwrap(); // truncated
        assert_eq!(cache.complete("4.atmos").unwrap(), None);
        write_atmos_entry(&path("5.atmos"), 480, json!({"bytes": 480 * 64 + 1}));
        assert_eq!(cache.complete("5.atmos").unwrap(), None);
        fs::create_dir(path("6.atmos")).unwrap();
        fs::write(path("6.atmos").join("atmos-916.f32le"), vec![0u8; 64]).unwrap(); // no summary yet: in progress
        assert_eq!(cache.complete("6.atmos").unwrap(), None);
        assert_eq!(cache.complete("7.atmos").unwrap(), None); // absent
    }

    #[test]
    fn complete_validates_stereo_and_binaural() {
        let scratch = Scratch::new();
        let cache = scratch.pcm(3);
        let path = |key: &str| cache.path(key).unwrap();
        write_stereo_entry(&path("1.stereo"), 1000, "stereo.f32le", None, true);
        assert_eq!(cache.complete("1.stereo").unwrap(), Some(path("1.stereo")));
        write_stereo_entry(&path("2.stereo"), 1000, "stereo.f32le", None, false);
        assert_eq!(cache.complete("2.stereo").unwrap(), None);
        write_stereo_entry(&path("3.stereo"), 1000, "stereo.f32le", Some(999), true);
        assert_eq!(cache.complete("3.stereo").unwrap(), None);
        write_stereo_entry(&path("4.binaural"), 1000, "binaural.f32le", None, true);
        assert_eq!(cache.complete("4.binaural").unwrap(), Some(path("4.binaural")));
        write_stereo_entry(&path("5.binaural"), 1000, "stereo.f32le", None, true); // wrong file name for the kind
        assert_eq!(cache.complete("5.binaural").unwrap(), None);
    }

    #[test]
    fn evict_keeps_recent_songs_with_all_variants() {
        let scratch = Scratch::new();
        let cache = scratch.pcm(2);
        for key in ["A.atmos", "A.stereo", "B.atmos", "C.atmos"] {
            let directory = cache.path(key).unwrap();
            if key.ends_with("stereo") {
                write_stereo_entry(&directory, 10, "stereo.f32le", None, true);
            } else {
                write_atmos_entry(&directory, 10, json!({}));
            }
        }
        cache.touch("B.atmos").unwrap();
        pause();
        cache.touch("A.stereo").unwrap();
        pause();
        cache.touch("A.atmos").unwrap();
        pause();
        cache.touch("C.atmos").unwrap();
        cache.evict(NO_KEYS).unwrap();
        assert_eq!(folders(&scratch.root.join("pcm")), ["A.atmos", "A.stereo", "C.atmos"]);
        assert_eq!(cache.entries().unwrap().into_keys().collect::<Vec<_>>(), ["A.atmos", "A.stereo", "C.atmos"]);
    }

    #[test]
    fn evict_protect() {
        let scratch = Scratch::new();
        let cache = scratch.pcm(1);
        for key in ["A.atmos", "B.atmos", "C.atmos"] {
            write_atmos_entry(&cache.path(key).unwrap(), 10, json!({}));
        }
        cache.touch("B.atmos").unwrap();
        pause();
        cache.touch("A.atmos").unwrap();
        pause();
        cache.touch("C.atmos").unwrap();
        cache.evict(["B.atmos"]).unwrap(); // B is protected and counts toward keep=1
        assert_eq!(folders(&scratch.root.join("pcm")), ["B.atmos"]);
    }

    #[test]
    fn entries_report_partial() {
        let scratch = Scratch::new();
        let cache = scratch.pcm(3);
        write_atmos_entry(&cache.path("1.atmos").unwrap(), 10, json!({}));
        let partial = cache.claim("2.atmos").unwrap();
        fs::write(partial.join("atmos-916.f32le"), vec![0u8; 640]).unwrap();
        let entries = cache.entries().unwrap();
        assert!(entries["1.atmos"].complete);
        assert!(!entries["2.atmos"].complete);
        assert_eq!(entries["2.atmos"].bytes, 640);
        assert!(cache.remove("2.atmos").unwrap());
        assert!(!cache.path("2.atmos").unwrap().exists());
    }

    // --- ArtCache

    #[test]
    fn put_get_and_names() {
        let scratch = Scratch::new();
        let cache = ArtCache::in_directory(scratch.root.join("art"), 10_000);
        cache.put("https://example.invalid/a.jpg", b"AAA").unwrap();
        assert_eq!(cache.get("https://example.invalid/a.jpg").unwrap().as_deref(), Some(b"AAA".as_slice()));
        assert_eq!(cache.get("https://example.invalid/b.jpg").unwrap(), None);
        let name = ArtCache::name_for("https://example.invalid/a.jpg");
        assert!(ArtCache::is_name(&name), "{name}");
        assert_eq!(mode(&scratch.root.join("art").join(&name)), 0o600);
        assert_eq!(cache.total_bytes().unwrap(), 3);
    }

    #[test]
    fn lru_eviction() {
        let scratch = Scratch::new();
        let cache = ArtCache::in_directory(scratch.root.join("art"), 250);
        cache.put("u1", &[0u8; 100]).unwrap();
        pause();
        cache.put("u2", &[0u8; 100]).unwrap();
        pause();
        assert!(cache.get("u1").unwrap().is_some());
        pause();
        cache.put("u3", &[0u8; 100]).unwrap();
        assert_eq!(cache.get("u2").unwrap(), None);
        assert!(cache.get("u1").unwrap().is_some());
        assert!(cache.get("u3").unwrap().is_some());
        assert_eq!(fs::read_dir(scratch.root.join("art")).unwrap().count(), 3); // two files + index.json
    }

    #[test]
    fn reload_sees_files() {
        let scratch = Scratch::new();
        let directory = scratch.root.join("art");
        ArtCache::in_directory(&directory, config::ART_CACHE_BYTES).put("u1", b"abc").unwrap();
        let reopened = ArtCache::in_directory(&directory, config::ART_CACHE_BYTES);
        assert_eq!(reopened.get("u1").unwrap().as_deref(), Some(b"abc".as_slice()));
    }

    // --- in-use markers: claim() refuses and evict() skips an entry another live process marked

    /// Another process of this user, ended when the test is over.
    struct OtherProcess(std::process::Child);

    impl OtherProcess {
        fn start() -> OtherProcess {
            OtherProcess(Command::new("sleep").arg("30").stdin(Stdio::null()).spawn().unwrap())
        }

        fn end(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    impl Drop for OtherProcess {
        fn drop(&mut self) {
            self.end();
        }
    }

    #[test]
    fn own_marker_does_not_block_this_process() {
        let scratch = Scratch::new();
        let cache = scratch.pcm(1);
        let entry = cache.path("A.atmos").unwrap();
        write_atmos_entry(&entry, 10, json!({}));
        cache.mark_inuse("A.atmos").unwrap();
        assert!(entry.join(format!(".inuse-{}", std::process::id())).is_file());
        assert!(!cache.in_use_elsewhere("A.atmos").unwrap());
        assert_eq!(cache.complete("A.atmos").unwrap(), Some(entry.clone()), "markers do not spoil validation");
        cache.claim("A.atmos").unwrap(); // our own marker never refuses us
        cache.unmark_inuse("A.atmos").unwrap();
        assert!(names(&entry, |item| item.file_name().to_string_lossy().starts_with(".inuse-")).is_empty());
    }

    #[test]
    fn live_foreign_marker_protects_the_entry() {
        let scratch = Scratch::new();
        let cache = scratch.pcm(1);
        let (entry_a, entry_b) = (cache.path("A.atmos").unwrap(), cache.path("B.atmos").unwrap());
        write_atmos_entry(&entry_a, 10, json!({}));
        write_atmos_entry(&entry_b, 10, json!({}));
        let mut other = OtherProcess::start();
        let marker = entry_a.join(format!(".inuse-{}", other.0.id()));
        fs::write(&marker, b"").unwrap();
        assert!(cache.in_use_elsewhere("A.atmos").unwrap());
        assert!(matches!(cache.claim("A.atmos"), Err(CacheError::Busy(_))));
        assert!(entry_a.join("render-summary.json").is_file(), "claim left the entry alone");
        cache.touch("B.atmos").unwrap();
        cache.evict(["B.atmos"]).unwrap(); // keep=1 would normally delete A
        assert!(entry_a.is_dir(), "an entry another live process uses survives eviction");
        assert!(entry_b.is_dir());
        other.end();
        assert!(!cache.in_use_elsewhere("A.atmos").unwrap(), "a dead pid's marker is stale");
        assert!(!marker.exists(), "and gets removed");
        cache.evict(["B.atmos"]).unwrap();
        assert!(!entry_a.exists());
        cache.claim("A.atmos").unwrap();
    }

    // --- beyond the Python tests

    #[test]
    fn caches_can_be_shared_between_threads() {
        fn shared<T: Send + Sync + 'static>() {}
        shared::<MediaCache>();
        shared::<PcmCache>();
        shared::<ArtCache>();
    }

    #[test]
    fn index_files_are_the_ones_python_writes_and_reads() {
        let scratch = Scratch::new();
        let cache = scratch.media(config::MEDIA_CACHE_BYTES);
        let meta = object(json!({"duration_s": 1.5, "codec": "eac3", "channels": 6, "profile": null}));
        cache.put("1234567890.atmos", &scratch.make_file("a", 1234), &meta).unwrap();
        let index = scratch.root.join("media/index.json");
        let text = fs::read_to_string(&index).unwrap();
        let (head, rest) = text.split_once("   \"last_access\": ").unwrap();
        let (last_access, tail) = rest.split_once(",\n").unwrap();
        assert_eq!(head, "{\n \"entries\": {\n  \"1234567890.atmos\": {\n");
        assert!((last_access.parse::<f64>().unwrap() - util::unix_now()).abs() < 60.0, "{last_access}");
        assert_eq!(
            tail,
            "   \"meta\": {\n    \"channels\": 6,\n    \"codec\": \"eac3\",\n    \"duration_s\": 1.5,\n    \
             \"profile\": null\n   },\n   \"size\": 1234\n  }\n },\n \"version\": 1\n}\n"
        );

        // an index exactly as Python's json.dump(indent=1, sort_keys=True) writes it: escapes, float spellings
        let written = "{\n \"entries\": {\n  \"../x\": {\n   \"last_access\": 1.0,\n   \"meta\": {},\n   \"size\": 5\n  },\n  \
                       \"1234567890.atmos\": {\n   \"last_access\": 1700000000.25,\n   \"meta\": {\n    \
                       \"bit_rate\": 768000,\n    \"duration_s\": 2.5e-05,\n    \"note\": \"caf\\u00e9\"\n   },\n   \
                       \"size\": 1\n  },\n  \"7.stereo\": {\n   \"last_access\": 1e+16,\n   \"meta\": {},\n   \
                       \"size\": 5\n  }\n },\n \"version\": 1\n}\n";
        fs::write(&index, written).unwrap();
        let reopened = scratch.media(config::MEDIA_CACHE_BYTES);
        let entries = reopened.entries().unwrap();
        assert_eq!(entries.keys().collect::<Vec<_>>(), ["1234567890.atmos"]); // records without a file are dropped
        let entry = &entries["1234567890.atmos"];
        assert_eq!((entry.size, entry.last_access), (1234, 1700000000.25)); // the size is the file's
        assert_eq!(entry.meta, object(json!({"bit_rate": 768000, "duration_s": 2.5e-05, "note": "café"})));

        let pcm = scratch.pcm(3);
        pcm.claim("1234567890.atmos").unwrap();
        let text = fs::read_to_string(scratch.root.join("pcm/index.json")).unwrap();
        assert!(text.starts_with("{\n \"entries\": {\n  \"1234567890.atmos\": {\n   \"last_access\": "), "{text}");
        assert!(text.ends_with("\n  }\n },\n \"version\": 1\n}\n"), "{text}");
        assert_eq!(mode(&scratch.root.join("pcm/index.json")), 0o600);
        // a garbled index costs the access times only
        fs::write(scratch.root.join("pcm/index.json"), "[1, 2").unwrap();
        assert_eq!(scratch.pcm(3).entries().unwrap().len(), 1);
        fs::write(scratch.root.join("pcm/index.json"), "{\"entries\": [1], \"version\": 1}").unwrap();
        assert_eq!(scratch.pcm(3).entries().unwrap().len(), 1);
    }

    #[test]
    fn a_cache_object_notices_another_writer() {
        let scratch = Scratch::new();
        let (player, tool) = (scratch.media(config::MEDIA_CACHE_BYTES), scratch.media(config::MEDIA_CACHE_BYTES));
        assert_eq!(player.total_bytes().unwrap(), 0);
        // a rewritten index is noticed by its modification time, and some file systems count
        // that in clock ticks: the writers take turns a few ticks apart
        let tick = || std::thread::sleep(Duration::from_millis(30));
        tool.put("1.atmos", &scratch.make_file("a", 10), &Map::new()).unwrap();
        tick();
        assert!(player.get("1.atmos").unwrap().is_some());
        tick();
        assert!(tool.remove("1.atmos").unwrap());
        tick();
        assert_eq!(player.entries().unwrap(), BTreeMap::new());
        // a staged file moves in from the staging folder and keeps nothing behind
        let staged = player.staging_path("2.stereo", ".decrypted.m4a").unwrap();
        fs::write(&staged, b"media").unwrap();
        let dest = player.put("2.stereo", &staged, &Map::new()).unwrap();
        assert_eq!((fs::read(&dest).unwrap(), staged.exists(), mode(&dest)), (b"media".to_vec(), false, 0o600));
        assert!(matches!(player.put("3.stereo", &scratch.root.join("absent"), &Map::new()), Err(CacheError::Io(_))));
        player.evict().unwrap();
        assert_eq!(player.total_bytes().unwrap(), 5);
        assert_eq!(MediaCache::new(scratch.config.clone()).directory, scratch.config.media_cache_dir);
    }

    #[test]
    fn pcm_keys_markers_and_kinds() {
        let scratch = Scratch::new();
        let cache = scratch.pcm(1);
        assert_eq!(cache.kind("1.lossless").unwrap(), "lossless");
        assert_eq!(cache.pcm_path("1.lossless").unwrap(), scratch.root.join("pcm/1.lossless/lossless.f32le"));
        // Hi-Res has a place of its own: it never takes the 48 kHz version's
        assert_eq!(cache.kind("1.hires").unwrap(), "hires");
        assert_eq!(cache.pcm_path("1.hires").unwrap(), scratch.root.join("pcm/1.hires/hires.f32le"));
        assert_eq!(cache.pcm_path("1.atmos").unwrap(), scratch.root.join("pcm/1.atmos/atmos-916.f32le"));
        assert_eq!(
            PcmCache::split_key("1.flac").unwrap_err().to_string(),
            "PCM cache key must end in one of ['atmos', 'binaural', 'hires', 'lossless', 'stereo']"
        );
        assert_eq!(cache.path("a..b").unwrap_err().to_string(), "cache key must be a plain '<id>.<variant>' string");
        assert!(key_matches(&format!("1{}", "a".repeat(200))) && !key_matches(&format!("1{}", "a".repeat(201))));
        cache.mark_inuse("1.atmos").unwrap(); // no directory: nothing happens
        cache.unmark_inuse("1.atmos").unwrap();
        assert!(!scratch.root.join("pcm/1.atmos").exists());
        assert!(!cache.remove("1.atmos").unwrap());
        // a marker that names no process, and one of a process that is gone
        let entry = cache.claim("1.atmos").unwrap();
        let mut gone = OtherProcess::start();
        let stale = entry.join(format!(".inuse-{}", gone.0.id()));
        gone.end();
        fs::write(&stale, b"").unwrap();
        fs::write(entry.join(".inuse-nobody"), b"").unwrap();
        assert!(!cache.in_use_elsewhere("1.atmos").unwrap());
        assert!(!stale.exists() && entry.join(".inuse-nobody").exists());
        // a lossless decode is validated like a stereo one; a protected key must be a PCM key
        write_stereo_entry(&cache.path("2.lossless").unwrap(), 100, "lossless.f32le", None, true);
        assert!(cache.complete("2.lossless").unwrap().is_some());
        assert_eq!(cache.complete("2.hires").unwrap(), None, "the 48 kHz decode is not the Hi-Res one");
        write_stereo_entry(&cache.path("2.hires").unwrap(), 100, "hires.f32le", None, true);
        assert!(cache.complete("2.hires").unwrap().is_some());
        assert!(cache.complete("2.lossless").unwrap().is_some(), "and the two stand side by side");
        cache.remove("2.hires").unwrap();
        assert!(matches!(cache.evict(["2.flac"]), Err(CacheError::Key(_))));
        cache.evict(["2.lossless"]).unwrap();
        assert_eq!(folders(&scratch.root.join("pcm")), ["2.lossless"]);
        assert_eq!(PcmCache::new(&scratch.config).directory, scratch.config.pcm_cache_dir);
    }

    #[test]
    fn art_touches_reach_the_index_in_batches() {
        let scratch = Scratch::new();
        let directory = scratch.root.join("art");
        let cache = ArtCache::in_directory(&directory, 150);
        cache.put("u1", &[1u8; 100]).unwrap();
        // hashlib.sha256(b"u1").hexdigest(): both engines keep an image under the same name
        let name = ArtCache::name_for("u1");
        assert_eq!(name, "bb82030dbc2bcaba32a90bf2e207a84a856fc5f033b77c480836ab6f77f40f19");
        assert!(directory.join(&name).is_file());
        let recorded = || recorded_last_access(recorded_entries(&directory.join("index.json")).get(&name));
        let written = recorded();
        assert!(written > 0.0);
        pause();
        assert!(cache.get("u1").unwrap().is_some()); // within the flush interval: the touch stays in memory
        assert_eq!(recorded(), written);
        assert!(cache.entries().unwrap()[&name].last_access > written);
        cache.flush().unwrap();
        assert!(recorded() > written);
        // a file that vanished is forgotten, and evict() writes that down
        fs::remove_file(directory.join(&name)).unwrap();
        assert_eq!(cache.get("u1").unwrap(), None);
        cache.evict().unwrap();
        assert!(recorded_entries(&directory.join("index.json")).is_empty());
        // files that are not images of this cache are left alone and not counted
        fs::write(directory.join("notes.txt"), b"x").unwrap();
        cache.put("u2", &[2u8; 100]).unwrap();
        cache.put("u3", &[3u8; 100]).unwrap();
        assert_eq!(cache.total_bytes().unwrap(), 100);
        assert!(directory.join("notes.txt").exists());
        assert_eq!(ArtCache::new(&scratch.config).directory, scratch.config.art_cache_dir);
    }
}
