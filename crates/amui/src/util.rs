//! Small helpers shared by the engine's modules: private files, a waitable flag, URL quoting.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch.
pub fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// A flag threads can set, clear and wait for (what `threading.Event` is in Python). Cancellation
/// of downloads, renders and jobs is one of these, shared in an `Arc`.
#[derive(Default)]
pub struct Event {
    state: Mutex<bool>,
    changed: Condvar,
}

impl Event {
    pub fn new() -> Event {
        Event::default()
    }

    pub fn set(&self) {
        *self.state.lock().unwrap() = true;
        self.changed.notify_all();
    }

    pub fn clear(&self) {
        *self.state.lock().unwrap() = false;
    }

    pub fn is_set(&self) -> bool {
        *self.state.lock().unwrap()
    }

    /// Block until the flag is set or `timeout` passes (`None` waits without limit). Returns
    /// whether the flag is set.
    pub fn wait(&self, timeout: Option<Duration>) -> bool {
        let mut set = self.state.lock().unwrap();
        match timeout {
            None => {
                while !*set {
                    set = self.changed.wait(set).unwrap();
                }
                true
            }
            Some(limit) => {
                let deadline = Instant::now() + limit;
                while !*set {
                    let now = Instant::now();
                    if now >= deadline {
                        return false;
                    }
                    set = self.changed.wait_timeout(set, deadline - now).unwrap().0;
                }
                true
            }
        }
    }
}

/// `~` and `~/...` resolved against `home`; anything else unchanged.
pub fn expand_user(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    }
}

/// Create `path` and its missing parents, each with mode 0700 (existing folders keep theirs).
pub fn create_private_dirs(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
}

/// Set a folder or file to owner-only access; failures are ignored (a foreign file system).
pub fn make_private(path: &Path, mode: u32) {
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

/// A new file that must not exist yet, readable and writable by the owner only.
pub fn create_private_new(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)
}

/// Replace `path` with `data` through a temporary file next to it (mode 0600): readers see the
/// old content or the new one, never a partial file.
pub fn write_private_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    // one name per call: two writers of the same file, in this process or another, never share
    // a temporary file
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_file_name(format!(".{name}.{}.{serial}.tmp", std::process::id()));
    let result = (|| {
        // a file left behind under this name by a process that died is written over
        let mut file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&temporary)?;
        file.write_all(data)?;
        file.flush()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// A JSON object read from a file, `None` when the file is missing, unreadable or something else.
pub fn read_json_object(path: &Path) -> Option<serde_json::Map<String, serde_json::Value>> {
    let text = fs::read_to_string(path).ok()?;
    match serde_json::from_str(&text).ok()? {
        serde_json::Value::Object(map) => Some(map),
        _ => None,
    }
}

/// Any JSON value read from a file, `None` when the file is missing, unreadable or not JSON.
pub fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn sorted(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            serde_json::Value::Object(keys.into_iter().map(|key| (key.clone(), sorted(&map[key]))).collect())
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// Atomic 0600 write of a state file: JSON with sorted keys, one space of indentation and a
/// final newline (the form every state file under the data folder has). Creates the folder
/// (0700) when it is missing.
pub fn write_private_json(path: &Path, value: &serde_json::Value) -> io::Result<()> {
    use serde::Serialize;
    if let Some(parent) = path.parent() {
        create_private_dirs(parent)?;
    }
    let mut text = Vec::new();
    let mut serializer =
        serde_json::Serializer::with_formatter(&mut text, serde_json::ser::PrettyFormatter::with_indent(b" "));
    sorted(value).serialize(&mut serializer).map_err(io::Error::other)?;
    text.push(b'\n');
    write_private_atomic(path, &text)
}

const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_.-~";

/// Python's `urllib.parse.quote`: every byte outside the unreserved set and `safe` becomes %XX.
pub fn quote(text: &str, safe: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        if UNRESERVED.contains(&byte) || safe.as_bytes().contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Python's `urllib.parse.quote_plus` with the default arguments: spaces become `+`.
pub fn quote_plus(text: &str) -> String {
    quote(text, " ").replace(' ', "+")
}

/// Python's `urllib.parse.urlencode` for string pairs, in the order given.
pub fn urlencode<K: AsRef<str>, V: AsRef<str>>(params: &[(K, V)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{}={}", quote_plus(key.as_ref()), quote_plus(value.as_ref())))
        .collect::<Vec<_>>()
        .join("&")
}

/// Resolve `reference` against `base` (RFC 3986, as `urllib.parse.urljoin` does for http URLs).
/// `None` when either is not a URL.
pub fn urljoin(base: &str, reference: &str) -> Option<String> {
    url::Url::parse(base).ok()?.join(reference).ok().map(String::from)
}

/// Python's `repr()` of a string, for the messages that quote a value with `{value!r}`: single
/// quotes, or double quotes when the text holds a single quote and no double one; the quote, the
/// backslash, control characters and white space other than the space escaped. Every other
/// character stays as it is (Python also escapes the invisible format characters and the
/// unassigned ones, such as a zero-width space).
pub fn python_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ' ' => out.push(' '),
            _ if character == quote => out.extend(['\\', quote]),
            _ if character.is_control() || character.is_whitespace() => {
                let code = character as u32;
                out.push_str(&if code < 0x100 { format!("\\x{code:02x}") } else { format!("\\u{code:04x}") });
            }
            _ => out.push(character),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn event_waits_and_wakes() {
        let event = Arc::new(Event::new());
        assert!(!event.wait(Some(Duration::from_millis(10))));
        let other = event.clone();
        let thread = std::thread::spawn(move || other.wait(Some(Duration::from_secs(5))));
        event.set();
        assert!(thread.join().unwrap());
        assert!(event.is_set());
        event.clear();
        assert!(!event.is_set());
    }

    #[test]
    fn quoting_matches_python() {
        assert_eq!(quote("a b/c?d", "/"), "a%20b/c%3Fd");
        assert_eq!(quote_plus("a b&c=d/é~"), "a+b%26c%3Dd%2F%C3%A9~");
        assert_eq!(urlencode(&[("ids[songs]", "1,2"), ("l", "en-GB")]), "ids%5Bsongs%5D=1%2C2&l=en-GB");
        assert_eq!(urljoin("https://h/a/b.m3u8?x=1", "c/d.m4a").as_deref(), Some("https://h/a/c/d.m4a"));
        assert_eq!(urljoin("https://h/a/b", "https://o/z").as_deref(), Some("https://o/z"));
    }

    #[test]
    fn private_files() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b");
        create_private_dirs(&nested).unwrap();
        assert_eq!(fs::metadata(&nested).unwrap().permissions().mode() & 0o777, 0o700);
        let file = nested.join("state.json");
        write_private_atomic(&file, b"{\"a\": 1}").unwrap();
        write_private_atomic(&file, b"{\"a\": 2}").unwrap();
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(read_json_object(&file).unwrap()["a"], 2);
        assert!(create_private_new(&file).is_err());
        let state = nested.join("deeper/eq.json");
        write_private_json(&state, &serde_json::json!({"b": [1, {"z": 1, "a": 2}], "a": "é"})).unwrap();
        assert_eq!(
            fs::read_to_string(&state).unwrap(),
            "{\n \"a\": \"é\",\n \"b\": [\n  1,\n  {\n   \"a\": 2,\n   \"z\": 1\n  }\n ]\n}\n"
        );
        assert_eq!(fs::metadata(&state).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(read_json(&state).unwrap()["b"][0], 1);
        assert!(read_json(&nested.join("missing.json")).is_none());
        fs::remove_dir_all(nested.join("deeper")).unwrap();
        assert_eq!(fs::read_dir(&nested).unwrap().count(), 1);
        assert_eq!(expand_user("~/x", Path::new("/home/u")), PathBuf::from("/home/u/x"));
    }

    #[test]
    fn repr_matches_python() {
        for (text, expected) in [
            ("zz", "'zz'"),
            ("x k", "'x k'"),
            ("", "''"),
            ("it's", "\"it's\""),
            ("say \"hi\"", "'say \"hi\"'"),
            ("it's \"both\"", "'it\\'s \"both\"'"),
            ("a\\b", "'a\\\\b'"),
            ("tab\there", "'tab\\there'"),
            ("line\nfeed\r", "'line\\nfeed\\r'"),
            ("\u{0}\u{1f}\u{7f}\u{85}", "'\\x00\\x1f\\x7f\\x85'"),
            ("café ~ é", "'café ~ é'"),
            ("\u{a0}\u{2003}", "'\\xa0\\u2003'"),
        ] {
            assert_eq!(python_repr(text), expected, "{text:?}");
        }
    }
}
