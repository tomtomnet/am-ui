//! The repository carries the tool sources and never a machine's private state.
//!
//! `workspace/` mixes tracked sources with what every machine creates for itself (the saved
//! session and its token, build products, reports, settings); `.gitignore` whitelists the
//! sources. These tests ask git itself, so they say so and pass outside a git checkout. The
//! scripts a fresh clone runs must be there, executable and parse.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The files of `workspace/` that are tracked; everything else there is a machine's own.
const TRACKED_WORKSPACE: [&str; 12] = [
    "workspace/README.md",
    "workspace/tools/music-web-probe.py",
    "workspace/tools/music_session.py",
    "workspace/tools/music-web-login.py",
    "workspace/tools/music_browser.py",
    "workspace/tools/downmix.py",
    "workspace/pipewire/applemusic-downmix.conf",
    "workspace/pipewire/applemusic-downmix.service",
    "workspace/pipewire/atmos-916-downmix.conf",
    "workspace/builds/music-downloader/amdl-linux/Cargo.toml",
    "workspace/builds/music-downloader/amdl-linux/Cargo.lock",
    "workspace/builds/music-downloader/amdl-linux/src/main.rs",
];

/// Paths of a machine's private state (they need not exist): all of them must be ignored.
const PRIVATE: [&str; 20] = [
    "workspace/builds/music-downloader/web-session/media-user-token",
    "workspace/builds/music-downloader/web-session/session-status.json",
    "workspace/builds/music-downloader/web-session/profile/Default/Cookies",
    "workspace/builds/music-downloader/amdl-android-src/rust/src/drm.rs",
    "workspace/builds/music-downloader/amdl-android-src/rust/test.wvd",
    "workspace/builds/music-downloader/amdl-patched/decrypt.rs",
    "workspace/builds/music-downloader/amdl-linux-target/release/amdl-linux-probe",
    "workspace/builds/music-downloader/amdl-linux-target/release/amdl-linux-probe.inputs",
    "workspace/builds/music-downloader/amdl-linux/target/release/amdl-linux-probe",
    "workspace/builds/music-downloader/dotnet-path.txt",
    "workspace/builds/mb-tmp/scratch.bin",
    "workspace/reports/music-web-login-20260101T000000Z/report.json",
    "workspace/pipewire/downmix-state.json",
    "workspace/tools/__pycache__/downmix.cpython-312.pyc",
    "workspace/tools/another-tool.py",
    "workspace/another-folder/file",
    "var/state/settings.json",
    "var/wrapper/data/account",
    "target/release/am-ui",
    "notes-local/notes.md",
];

/// The scripts of a fresh clone; `build-env.sh` is sourced, never run.
const SCRIPTS: [&str; 7] = [
    "am-ui",
    "setup.sh",
    "tools/drm-helper/build.sh",
    "tools/cavern-lab/build.sh",
    "tools/cavern-lab/build-env.sh",
    "tools/cavern-lab/render-music-916-lab.sh",
    "tools/lossless/build.sh",
];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("the repository folder")
}

/// `git -C <root> args`: (exit code, standard output); `None` without git or outside a checkout.
fn git(args: &[&str]) -> Option<(i32, String)> {
    let root = root();
    if !root.join(".git").exists() {
        return None;
    }
    let output = Command::new("git").arg("-C").arg(&root).args(args).output().ok()?;
    Some((output.status.code().unwrap_or(-1), String::from_utf8_lossy(&output.stdout).into_owned()))
}

#[test]
fn private_state_is_ignored() {
    for path in PRIVATE {
        let Some((code, _)) = git(&["check-ignore", "-q", path]) else {
            eprintln!("note: not a git checkout (or no git): nothing to check");
            return;
        };
        assert_eq!(code, 0, "{path} is not ignored");
    }
}

#[test]
fn tool_sources_are_not_ignored() {
    for path in TRACKED_WORKSPACE {
        let Some((code, _)) = git(&["check-ignore", "-q", path]) else { return };
        assert_eq!(code, 1, "{path} is ignored");
        assert!(root().join(path).is_file(), "{path} is missing");
    }
}

#[test]
fn nothing_private_is_tracked() {
    let Some((code, listing)) = git(&["ls-files", "-z"]) else { return };
    assert_eq!(code, 0);
    for path in listing.split('\0').filter(|path| !path.is_empty()) {
        let first = path.split('/').next().unwrap_or_default();
        assert!(!matches!(first, "var" | "notes-local" | "target"), "{path} is tracked");
        if first == "workspace" {
            assert!(TRACKED_WORKSPACE.contains(&path), "{path} is tracked but not a workspace source");
        }
        assert!(!path.contains("web-session") && !path.contains("media-user-token"), "{path} is tracked");
    }
}

#[test]
fn scripts_are_there_executable_and_parse() {
    use std::os::unix::fs::PermissionsExt;
    let root = root();
    let bash = Command::new("bash").arg("--version").output().is_ok();
    for path in SCRIPTS {
        let script = root.join(path);
        let mode = std::fs::metadata(&script).unwrap_or_else(|_| panic!("{path} is missing")).permissions().mode();
        if path != "tools/cavern-lab/build-env.sh" {
            assert_ne!(mode & 0o111, 0, "{path} is not executable");
        }
        if bash {
            let checked = Command::new("bash").arg("-n").arg(&script).output().expect("bash runs");
            assert!(checked.status.success(), "{path}: {}", String::from_utf8_lossy(&checked.stderr));
        }
    }
}
