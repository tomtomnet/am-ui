//! The playback log and the output's own log file.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use super::LOG_MAX_BYTES;
use crate::config::Config;
use crate::loudness::lock;
use crate::util;

/// Local time as `datetime.now().isoformat(timespec="milliseconds")` prints it.
pub(super) fn timestamp() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = now.as_secs() as libc::time_t;
    // SAFETY: `tm` is plain data for which all zeroes is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call, which keeps neither.
    unsafe { libc::localtime_r(&seconds, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        now.subsec_millis()
    )
}

/// A log file for appending, owner-only, in an owner-only folder.
pub(super) fn open_log_file(path: &Path, truncate: bool) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        util::create_private_dirs(parent)?;
    }
    let mut options = OpenOptions::new();
    options.append(true).create(true).mode(0o600);
    if truncate {
        options.custom_flags(libc::O_TRUNC);
    }
    options.open(path)
}

struct LogFile {
    path: PathBuf,
    file: Option<File>,
}

impl LogFile {
    fn open(&mut self) -> bool {
        if self.file.is_none() {
            self.file = open_log_file(&self.path, false).ok();
        }
        self.file.is_some()
    }
}

/// Timestamped 0600 log, rotated to `<name>.1` at [`LOG_MAX_BYTES`]; shared by the output and
/// the player.
pub struct PlaybackLog {
    state: Mutex<LogFile>,
}

impl PlaybackLog {
    pub fn new(path: impl Into<PathBuf>) -> PlaybackLog {
        PlaybackLog { state: Mutex::new(LogFile { path: path.into(), file: None }) }
    }

    /// The engine's playback log: `playback.log` in the log folder.
    pub fn for_config(config: &Config) -> PlaybackLog {
        PlaybackLog::new(config.log_dir.join("playback.log"))
    }

    pub fn path(&self) -> PathBuf {
        lock(&self.state).path.clone()
    }

    /// Continue in another file (the current one is closed).
    pub fn set_path(&self, path: impl Into<PathBuf>) {
        let mut state = lock(&self.state);
        state.file = None;
        state.path = path.into();
    }

    pub fn close(&self) {
        lock(&self.state).file = None;
    }

    /// Append `line` with the local time in front. A log that cannot be written is skipped.
    pub fn write(&self, line: &str) {
        let data = format!("{} {line}\n", timestamp());
        let mut state = lock(&self.state);
        if !state.open() {
            return;
        }
        let result = (|| -> io::Result<()> {
            let size = state.file.as_ref().map(File::metadata).transpose()?.map_or(0, |meta| meta.len());
            if size + data.len() as u64 > LOG_MAX_BYTES {
                state.file = None;
                let mut rotated = state.path.clone().into_os_string();
                rotated.push(".1");
                fs::rename(&state.path, rotated)?;
                if !state.open() {
                    return Ok(());
                }
            }
            match state.file.as_mut() {
                Some(file) => file.write_all(data.as_bytes()),
                None => Ok(()),
            }
        })();
        if result.is_err() {
            state.file = None;
        }
    }
}

/// The output's own log (`output.log`): what the consumer says about itself, started over when it
/// has grown past [`LOG_MAX_BYTES`].
pub(super) struct ConsumerLog {
    path: PathBuf,
    file: Mutex<Option<File>>,
}

impl ConsumerLog {
    pub(super) fn new(path: PathBuf) -> ConsumerLog {
        ConsumerLog { path, file: Mutex::new(None) }
    }

    pub(super) fn open(&self) {
        let oversized = fs::metadata(&self.path).is_ok_and(|meta| meta.len() > LOG_MAX_BYTES);
        *lock(&self.file) = open_log_file(&self.path, oversized).ok();
    }

    pub(super) fn close(&self) {
        *lock(&self.file) = None;
    }

    pub(super) fn write(&self, line: &str) {
        if let Some(file) = lock(&self.file).as_mut() {
            let _ = file.write_all(format!("{} {line}\n", timestamp()).as_bytes());
        }
    }
}
