//! Paths and constants shared by the engine and the application.
//!
//! Nothing here touches the network or reads a secret. The engine uses a few things from a
//! "workspace" folder and lets them keep their state there: the saved browser session, a DRM
//! helper, an older render script. The repository carries that folder (`workspace/`, see its
//! README); `setup.sh` builds what has to be built. Which folder is used:
//!
//! 1. `AMUI_WORKSPACE`, when it is set and holds the tools;
//! 2. an existing `~/Documents/apple-music-gpu` that holds them (a machine that already has a
//!    saved session there keeps it);
//! 3. the bundled `<am-ui>/workspace`.
//!
//! The DRM helper is the exception: the build this repository makes (`tools/drm-helper/build.sh`,
//! in the bundled workspace) is used whenever it exists, whichever workspace is active, because
//! the engine and that build agree on what they say to each other. A workspace's own helper is
//! used only without it.
//!
//! `AMUI_VAR` moves this application's private runtime data (caches, logs, state, built
//! toolchains). `AMUI_ROOT` names the am-ui folder when the program is not run from inside it.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::RwLock;

use crate::util;

// --- audio route: must match the downmix sink's channel order (workspace/pipewire/atmos-916-downmix.conf)
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 16;
pub const CHANNEL_MAP: [&str; CHANNELS] =
    ["FL", "FR", "FC", "LFE", "SL", "SR", "RL", "RR", "FLW", "FRW", "TFL", "TFR", "TSL", "TSR", "TRL", "TRR"];
/// Interleaved little-endian float32.
pub const FRAME_BYTES: usize = CHANNELS * 4;
/// The 16-channel "Atmos 9.1.6 Downmix" filter-chain sink.
pub const SINK_NAME: &str = "atmos_916_downmix";
pub const PW_APP_NAME: &str = "Apple Music";
pub const PW_NODE_NAME: &str = "am-ui";

// --- Apple hosts. Account credentials go only to these two (see `web`).
pub const API_HOST: &str = "amp-api.music.apple.com";
pub const LICENSE_HOST: &str = "play.itunes.apple.com";
pub const LICENSE_URL: &str = "https://play.itunes.apple.com/WebObjects/MZPlay.woa/wa/acquireWebPlaybackLicense";
pub const WEB_PLAYBACK_URL: &str = "https://play.itunes.apple.com/WebObjects/MZPlay.woa/wa/webPlayback";

// --- playback tuning
/// Seconds of audio rendered before playback of a track starts (the render must also be
/// producing audio at three times real time or more: `player::RENDER_START_RATE`). The renderer
/// has its first second out about half a second after it starts and runs about 25 times real
/// time once warm; the first Sound Check estimate is made from this much audio too and refined,
/// gently, as more comes.
pub const MIN_BUFFER_SECONDS: f64 = 1.0;
/// The output's assumed delay after a pipe consumer (its reader plays what it read this much
/// later), and before a consumer has said where it plays.
pub const OUTPUT_LATENCY_MS: u32 = 50;
/// How far the feeder keeps the pipe ahead of the audio clock.
pub const OUTPUT_AHEAD_MS: u32 = 150;
/// Single-song download cap.
pub const MEDIA_MAX_BYTES: u64 = 256 * 1024 * 1024;

// --- caches
/// Decrypted media kept (least recently used goes first).
pub const MEDIA_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Renders kept: current, next, previous (each about 3 MB per second of audio).
pub const PCM_CACHE_KEEP: usize = 3;
pub const ART_CACHE_BYTES: u64 = 256 * 1024 * 1024;

// --- renderer options of the patched Cavern (tools/cavern-lab)
/// The decoder fixes: the JOC timeslot wrap, the matrix interpolation of the specification, the
/// 90° phase of the surround channels in downmix configuration 3, and the LFE's alignment. The
/// decoder's LFE does not pass the QMF bank that the JOC reconstruction puts the other channels
/// through (577 samples of delay), and Cavern gives it the objects' -3 dB. Against a recording of
/// Apple's own render through the same fold-down, the delayed LFE at +3 dB brings the correlation
/// of the bass from 0.54 to 0.998 and the band levels within 0.2 dB.
pub const LAB_FIXES: &str =
    "--joc-timeslot-fix --joc-interp-fix --surround-phase -1 --lfe-gain 1.41421356 --lfe-delay 577";
/// Bass management: the low frequencies of every channel into the LFE channel as well. Apple's
/// render has no such feed (off by default); before the LFE was aligned, it made up for the bass
/// that the early LFE cancelled.
pub const LAB_BASS_FEED: &str = "--bass-manage 0.35 90";

/// A file every workspace with the engine tools has.
pub const WORKSPACE_MARKER: &str = "tools/music-web-probe.py";
const DRM_HELPER_PATH: &str = "builds/music-downloader/amdl-linux-target/release/amdl-linux-probe";

#[derive(Debug, Clone)]
struct RenderOptions {
    bass_manage: bool,
    lab_render_options: String,
    render_generation: String,
}

/// Every path the engine uses, resolved once at start-up.
#[derive(Debug)]
pub struct Config {
    /// The am-ui folder (holds `workspace/`, `tools/` and by default `var/`).
    pub root: PathBuf,
    pub bundled_workspace: PathBuf,
    pub legacy_workspace: PathBuf,
    /// The workspace in use and where that choice came from.
    pub workspace: PathBuf,
    pub workspace_source: String,
    /// This application's private runtime data.
    pub var: PathBuf,

    // --- the workspace: its own state (the token is never copied elsewhere)
    pub stage: PathBuf,
    pub web_session_dir: PathBuf,
    /// The browser sign-in's token; read only through `web::Web`.
    pub token_file: PathBuf,
    /// Last check of the browser sign-in; no secrets inside.
    pub session_status_file: PathBuf,
    /// What `tools/drm-helper/build.sh` (`./setup.sh`) builds.
    pub own_drm_helper: PathBuf,
    /// Widevine challenge/decrypt helper in use.
    pub drm_helper: PathBuf,
    /// Stock Cavern probe of an older workspace, when there is one.
    pub workspace_render_script: PathBuf,

    // --- renderer: am-ui's patched Cavern (tools/cavern-lab) when built
    pub lab_render_script: PathBuf,
    pub lab_probe_dll: PathBuf,
    pub render_script: PathBuf,
    generation_base: String,
    render: RwLock<RenderOptions>,

    // --- private runtime data (created 0700 by ensure_dirs())
    /// Decrypted .m4a files, one per catalog song id and variant.
    pub media_cache_dir: PathBuf,
    /// Renders and decodes: `<song id>/atmos-916.f32le` + `render-summary.json`.
    pub pcm_cache_dir: PathBuf,
    /// Artwork bytes keyed by a hash of the URL.
    pub art_cache_dir: PathBuf,
    /// Queue, settings, small API caches.
    pub state_dir: PathBuf,
    /// Last check of the native sign-in: state and time only.
    pub android_session_status_file: PathBuf,
    pub log_dir: PathBuf,
    /// Encrypted downloads in flight, helper scratch.
    pub tmp_dir: PathBuf,

    // --- lossless (ALAC) route: the local FairPlay wrapper service (tools/lossless)
    pub lossless_port: u16,
    pub lossless_url: String,
    pub lossless_build_dir: PathBuf,
    /// Built by `tools/lossless/build.sh`.
    pub lossless_rip_helper: PathBuf,
    /// The Python version of the wrapper control helper (the Python application's).
    pub lossless_wrapperctl: PathBuf,
    /// The wrapper control helper as a command line without its command. A Rust program of
    /// am-ui runs itself in that role ([`Config::run_helper_as`]); empty (tests), it is the
    /// Python script [`Config::lossless_wrapperctl`] under `python3 -I`.
    pub wrapper_helper: Vec<OsString>,
    /// The wrapper's account state (0700). The engine reads one file of it, the music user
    /// token (`web`, sent to Apple's two API hosts only), and looks at the others' presence.
    pub lossless_data_dir: PathBuf,
    /// Highest ALAC sample rate to fetch (`AMUI_LOSSLESS_MAX_RATE`; the output chain runs at
    /// [`SAMPLE_RATE`], higher rates are resampled for playback).
    pub lossless_max_rate: u32,

    /// `AMUI_SIGNIN=browser` puts the browser sign-in before the native one.
    pub prefer_browser_signin: bool,

    /// `CAVERN_LAB_OPTIONS`: switches for the lab renderer in place of the recommended ones (the
    /// render script's documented override, for experiments; an empty value means no switches).
    pub lab_options_override: Option<String>,

    /// The language of the desktop session as a language tag ("en-US"): the first of
    /// `AMUI_LANGUAGE`, `LC_ALL`, `LC_MESSAGES` and `LANG` that names one. Apple's own words
    /// (the titles of shelves, notes) are asked for in it where the catalog has them in it.
    pub language: Option<String>,
}

/// A locale name ("en_US.UTF-8", "de_DE@euro", "ja") as a language tag ("en-US", "de-DE", "ja");
/// `None` for the names that stand for no language ("C", "POSIX") and for anything else.
fn language_tag(locale: &str) -> Option<String> {
    let name = locale.split(['.', '@']).next().unwrap_or_default().trim();
    let mut parts = name.split(['_', '-']);
    let language =
        parts.next().filter(|part| (2..=3).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_alphabetic()))?;
    let mut tag = language.to_ascii_lowercase();
    for part in parts {
        if !(2..=8).contains(&part.len()) || !part.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        // a region in capitals, a script as it is
        tag.push('-');
        tag.push_str(&if part.len() == 2 { part.to_ascii_uppercase() } else { part.to_string() });
    }
    Some(tag)
}

impl Config {
    /// The configuration of this process: the am-ui folder found from `AMUI_ROOT`, the program's
    /// own location or the source tree, and the process environment.
    pub fn from_process() -> Config {
        // a variable whose name is not text is of no concern here; a value that is not is read
        // as well as it can be (`std::env::vars()` would stop the program)
        let env: HashMap<String, String> = std::env::vars_os()
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value.to_string_lossy().into_owned())))
            .collect();
        let home = env.get("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
        Config::new(discover_root(&env), &home, &env)
    }

    /// The configuration for an am-ui folder, a home folder and an environment.
    pub fn new(root: PathBuf, home: &Path, env: &HashMap<String, String>) -> Config {
        let get = |key: &str| env.get(key).map(String::as_str);
        let bundled_workspace = root.join("workspace");
        let legacy_workspace = home.join("Documents/apple-music-gpu");
        let (workspace, workspace_source) =
            resolve_workspace(get("AMUI_WORKSPACE"), &bundled_workspace, &legacy_workspace, home);
        let var = match get("AMUI_VAR") {
            Some(value) if !value.is_empty() => util::expand_user(value, home),
            _ => root.join("var"),
        };

        let stage = workspace.join("builds/music-downloader");
        let web_session_dir = stage.join("web-session");
        let own_drm_helper = bundled_workspace.join(DRM_HELPER_PATH);
        let drm_helper = resolve_drm_helper(&own_drm_helper, &workspace);
        let workspace_render_script = workspace.join("tools/render-music-916.sh");

        let lab_render_script = root.join("tools/cavern-lab/render-music-916-lab.sh");
        let lab_probe_dll = root.join("tools/cavern-lab/probe/bin/Release/net8.0/CavernProbe.dll");
        let (render_script, generation_base) = match get("AMUI_RENDER_SCRIPT") {
            Some(script) if !script.is_empty() => (util::expand_user(script, home), "custom"),
            _ if lab_probe_dll.is_file() => (lab_render_script.clone(), "cavern-lab-2"),
            _ if workspace_render_script.is_file() => (workspace_render_script.clone(), "workspace-1"),
            // nothing built yet: workspace_ready() and the doctor say "run ./setup.sh"
            _ => (lab_render_script.clone(), "cavern-lab-2"),
        };
        let bass_manage =
            matches!(get("AMUI_BASS_MANAGE").unwrap_or("off").to_lowercase().as_str(), "on" | "1" | "yes" | "true");

        let state_dir = var.join("state");
        let lossless_port = get("LOSSLESS_PORT").and_then(|v| v.parse().ok()).unwrap_or(12351);
        let lossless_build_dir = var.join("build/lossless");
        let config = Config {
            stage: stage.clone(),
            token_file: web_session_dir.join("media-user-token"),
            session_status_file: web_session_dir.join("session-status.json"),
            web_session_dir,
            own_drm_helper,
            drm_helper,
            workspace_render_script,
            lab_render_script,
            lab_probe_dll,
            render_script,
            generation_base: generation_base.to_string(),
            render: RwLock::new(RenderOptions {
                bass_manage,
                lab_render_options: String::new(),
                render_generation: String::new(),
            }),
            media_cache_dir: var.join("cache/media"),
            pcm_cache_dir: var.join("cache/pcm"),
            art_cache_dir: var.join("cache/art"),
            android_session_status_file: state_dir.join("android-session.json"),
            state_dir,
            log_dir: var.join("logs"),
            tmp_dir: var.join("tmp"),
            lossless_port,
            lossless_url: format!("http://127.0.0.1:{lossless_port}"),
            lossless_rip_helper: lossless_build_dir.join("bin/amui-fairplay-rip"),
            lossless_build_dir,
            lossless_wrapperctl: root.join("tools/lossless/wrapperctl.py"),
            wrapper_helper: Vec::new(),
            lossless_data_dir: var.join("wrapper/rootfs/data"),
            lossless_max_rate: get("AMUI_LOSSLESS_MAX_RATE").and_then(|v| v.parse().ok()).unwrap_or(48_000),
            prefer_browser_signin: get("AMUI_SIGNIN").is_some_and(|v| v.trim().eq_ignore_ascii_case("browser")),
            lab_options_override: get("CAVERN_LAB_OPTIONS").map(str::to_string),
            language: ["AMUI_LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
                .into_iter()
                .find_map(|key| language_tag(get(key)?)),
            root,
            bundled_workspace,
            legacy_workspace,
            workspace,
            workspace_source,
            var,
        };
        config.set_bass_manage(bass_manage);
        config
    }

    /// Switch the bass-managed LFE feed (lab renderer only) at run time; recomputes the render
    /// options and the generation string, so cached renders of the other setting are re-rendered.
    pub fn set_bass_manage(&self, enabled: bool) {
        let lab = self.render_script == self.lab_render_script;
        let mut render = self.render.write().unwrap();
        render.bass_manage = enabled;
        render.lab_render_options =
            if enabled { format!("{LAB_FIXES} {LAB_BASS_FEED}") } else { LAB_FIXES.to_string() };
        render.render_generation = format!("{}{}", self.generation_base, if lab && enabled { "+bm" } else { "" });
    }

    pub fn bass_manage(&self) -> bool {
        self.render.read().unwrap().bass_manage
    }

    /// Options passed to the lab renderer: the recommended ones for the current bass-management
    /// setting, or the override from the environment.
    pub fn lab_render_options(&self) -> String {
        match &self.lab_options_override {
            Some(options) => options.clone(),
            None => self.render.read().unwrap().lab_render_options.clone(),
        }
    }

    /// Renders in the PCM cache carry the generation that produced them; a different one is
    /// re-rendered.
    pub fn render_generation(&self) -> String {
        self.render.read().unwrap().render_generation.clone()
    }

    /// Run the wrapper control helper as `program` with `args` before its command (`amui
    /// wrapper`, `am-ui --wrapper`) instead of the Python script.
    pub fn run_helper_as(&mut self, program: impl Into<OsString>, args: &[&str]) {
        self.wrapper_helper = std::iter::once(program.into()).chain(args.iter().map(OsString::from)).collect();
    }

    /// The helper's command line without its command.
    fn helper(&self) -> Vec<OsString> {
        if self.wrapper_helper.is_empty() {
            vec!["python3".into(), "-I".into(), self.lossless_wrapperctl.clone().into()]
        } else {
            self.wrapper_helper.clone()
        }
    }

    /// The helper's command line with `arguments` after it, ready to be started.
    pub fn helper_command(&self, arguments: &[&str]) -> Command {
        let helper = self.helper();
        let mut command = Command::new(&helper[0]);
        command.args(&helper[1..]).args(arguments);
        command
    }

    /// The helper's command line with `arguments` after it, as text.
    pub fn helper_words(&self, arguments: &[&str]) -> Vec<String> {
        let words = self.helper().into_iter().map(|word| word.to_string_lossy().into_owned());
        words.chain(arguments.iter().map(|word| word.to_string())).collect()
    }

    /// Whether the helper is there: the program that runs it, or the script Python runs.
    pub fn helper_present(&self) -> bool {
        match self.wrapper_helper.first() {
            Some(program) => Path::new(program).is_file(),
            None => self.lossless_wrapperctl.is_file(),
        }
    }

    /// True when Apple's native sign-in can run here: the lossless route is built and its
    /// runtime staged (`./setup.sh` does both on x86_64 unless `--no-lossless`). Filesystem only.
    pub fn native_signin_built(&self) -> bool {
        self.helper_present()
            && self.lossless_build_dir.join("bin/wrapper-lite-rootless").is_file()
            && self.lossless_data_dir.parent().is_some_and(|rootfs| rootfs.join("system/bin/lite").is_file())
    }

    /// Create the private runtime directories with owner-only permissions (every level below
    /// the data folder).
    pub fn ensure_dirs(&self) -> io::Result<()> {
        util::create_private_dirs(&self.var)?;
        let leaves = [
            &self.media_cache_dir,
            &self.pcm_cache_dir,
            &self.art_cache_dir,
            &self.state_dir,
            &self.log_dir,
            &self.tmp_dir,
        ];
        for path in leaves {
            util::create_private_dirs(path)?;
        }
        util::make_private(&self.var, 0o700);
        if let Some(cache) = self.media_cache_dir.parent() {
            util::make_private(cache, 0o700);
        }
        for path in leaves {
            util::make_private(path, 0o700);
        }
        Ok(())
    }

    /// Human-readable problems with what the engine needs on disk (empty = all present).
    pub fn workspace_ready(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if !self.drm_helper.exists() {
            problems.push(format!("DRM helper not built: {} (run ./setup.sh)", self.drm_helper.display()));
        }
        if self.render_script == self.lab_render_script && !self.lab_probe_dll.is_file() {
            problems.push("Atmos renderer not built: run ./setup.sh (or tools/cavern-lab/build.sh)".to_string());
        } else if !self.render_script.exists() {
            problems.push(format!("render script missing: {}", self.render_script.display()));
        }
        problems
    }
}

/// (folder, where the choice came from). A folder without the tools is reported by
/// `workspace_ready()` and the doctor.
fn resolve_workspace(named: Option<&str>, bundled: &Path, legacy: &Path, home: &Path) -> (PathBuf, String) {
    let named = named.unwrap_or("").trim();
    if !named.is_empty() {
        let chosen = util::expand_user(named, home);
        if chosen.join(WORKSPACE_MARKER).is_file() || !bundled.join(WORKSPACE_MARKER).is_file() {
            return (chosen, "AMUI_WORKSPACE".to_string());
        }
        return (bundled.to_path_buf(), format!("bundled (AMUI_WORKSPACE={named} holds no engine tools)"));
    }
    if legacy.join(WORKSPACE_MARKER).is_file() {
        return (legacy.to_path_buf(), "existing workspace".to_string());
    }
    (bundled.to_path_buf(), "bundled".to_string())
}

/// This repository's build when there is one, else the active workspace's helper.
fn resolve_drm_helper(own: &Path, workspace: &Path) -> PathBuf {
    if own.is_file() { own.to_path_buf() } else { workspace.join(DRM_HELPER_PATH) }
}

fn is_root(path: &Path) -> bool {
    path.join("tools/drm-helper/build.sh").is_file() && path.join("workspace").is_dir()
}

/// The am-ui folder: `AMUI_ROOT`, else the nearest folder above the running program that holds
/// the repository's tools, else the same search from where this crate was built.
fn discover_root(env: &HashMap<String, String>) -> PathBuf {
    if let Some(named) = env.get("AMUI_ROOT").filter(|v| !v.is_empty()) {
        return PathBuf::from(named);
    }
    let program = std::env::current_exe().ok().and_then(|exe| exe.canonicalize().ok());
    let candidates = [program, Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")))];
    for start in candidates.iter().flatten() {
        if let Some(found) = start.ancestors().find(|folder| is_root(folder)) {
            return found.to_path_buf();
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    struct Scratch {
        dir: tempfile::TempDir,
    }

    impl Scratch {
        fn new() -> Scratch {
            Scratch { dir: tempfile::tempdir().unwrap() }
        }

        fn folder(&self, name: &str, tools: bool) -> PathBuf {
            let path = self.dir.path().join(name);
            fs::create_dir_all(&path).unwrap();
            if tools {
                fs::create_dir_all(path.join(WORKSPACE_MARKER).parent().unwrap()).unwrap();
                fs::write(path.join(WORKSPACE_MARKER), "").unwrap();
            }
            path
        }

        fn resolve(
            &self,
            named: Option<&str>,
            legacy_tools: bool,
            bundled_tools: bool,
        ) -> ((PathBuf, String), PathBuf, PathBuf) {
            let legacy = self.folder("legacy", legacy_tools);
            let bundled = self.folder("bundled", bundled_tools);
            (resolve_workspace(named, &bundled, &legacy, Path::new("/nonexistent-home")), legacy, bundled)
        }
    }

    #[test]
    fn fresh_machine_uses_the_bundled_workspace() {
        let scratch = Scratch::new();
        let ((path, source), _legacy, bundled) = scratch.resolve(None, false, true);
        assert_eq!((path, source.as_str()), (bundled, "bundled"));
    }

    #[test]
    fn existing_private_workspace_is_preferred() {
        let scratch = Scratch::new();
        let ((path, source), legacy, _bundled) = scratch.resolve(None, true, true);
        assert_eq!((path, source.as_str()), (legacy, "existing workspace"));
    }

    #[test]
    fn named_workspace_with_tools_wins() {
        let scratch = Scratch::new();
        let named = scratch.folder("named", true);
        let ((path, source), _legacy, _bundled) = scratch.resolve(named.to_str(), true, true);
        assert_eq!((path, source.as_str()), (named, "AMUI_WORKSPACE"));
    }

    /// A stale AMUI_WORKSPACE (pointing at the clone itself, say) must not break the engine.
    #[test]
    fn named_folder_without_tools_falls_back_to_the_bundled_one() {
        let scratch = Scratch::new();
        let named = scratch.folder("clone-root", false);
        let ((path, source), _legacy, bundled) = scratch.resolve(named.to_str(), false, true);
        assert_eq!(path, bundled);
        assert!(source.contains("AMUI_WORKSPACE") && source.contains("bundled"));
    }

    #[test]
    fn named_folder_is_kept_when_nothing_else_has_the_tools() {
        let scratch = Scratch::new();
        let named = scratch.folder("named", false);
        let ((path, source), _legacy, _bundled) = scratch.resolve(named.to_str(), false, false);
        assert_eq!((path, source.as_str()), (named, "AMUI_WORKSPACE"));
    }

    #[test]
    fn blank_value_counts_as_unset() {
        let scratch = Scratch::new();
        let ((path, source), _legacy, bundled) = scratch.resolve(Some("  "), false, true);
        assert_eq!((path, source.as_str()), (bundled, "bundled"));
    }

    fn config_in(scratch: &Scratch, env: &[(&str, &str)]) -> Config {
        let root = scratch.dir.path().join("am-ui");
        fs::create_dir_all(root.join("workspace")).unwrap();
        let env = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::new(root, &scratch.dir.path().join("home"), &env)
    }

    #[test]
    fn workspace_state_lives_inside_the_workspace() {
        let scratch = Scratch::new();
        let config = config_in(&scratch, &[]);
        assert_eq!(config.workspace, config.bundled_workspace);
        for path in [&config.token_file, &config.session_status_file, &config.workspace_render_script] {
            assert!(path.starts_with(&config.workspace), "{path:?}");
        }
        assert_eq!(config.var, config.root.join("var"));
        assert_eq!(config.lossless_url, "http://127.0.0.1:12351");
        assert_eq!(config.lossless_max_rate, 48_000);
        assert!(!config.prefer_browser_signin);
    }

    #[test]
    fn drm_helper_is_this_repository_s_build_when_there_is_one() {
        let scratch = Scratch::new();
        let own = scratch.dir.path().join("own/helper");
        let other = scratch.dir.path().join("other-workspace");
        assert_eq!(resolve_drm_helper(&own, &other), other.join(DRM_HELPER_PATH)); // not built here: the workspace's
        fs::create_dir_all(own.parent().unwrap()).unwrap();
        fs::write(&own, b"").unwrap();
        assert_eq!(resolve_drm_helper(&own, &other), own);
        let config = config_in(&scratch, &[]);
        assert_eq!(config.own_drm_helper, config.bundled_workspace.join(DRM_HELPER_PATH));
        assert_eq!(config.drm_helper, config.workspace.join(DRM_HELPER_PATH));
    }

    #[test]
    fn environment_moves_data_and_ports() {
        let scratch = Scratch::new();
        let data = scratch.dir.path().join("data");
        let config = config_in(
            &scratch,
            &[
                ("AMUI_VAR", data.to_str().unwrap()),
                ("LOSSLESS_PORT", "12399"),
                ("AMUI_LOSSLESS_MAX_RATE", "192000"),
                ("AMUI_SIGNIN", " Browser "),
                ("AMUI_BASS_MANAGE", "on"),
            ],
        );
        assert_eq!(config.var, data);
        assert_eq!(config.state_dir, data.join("state"));
        assert_eq!((config.lossless_port, config.lossless_url.as_str()), (12399, "http://127.0.0.1:12399"));
        assert_eq!(config.lossless_max_rate, 192_000);
        assert!(config.prefer_browser_signin);
        assert!(config.bass_manage());
    }

    #[test]
    fn the_wrapper_helper_is_the_python_script_until_a_program_runs_itself() {
        let scratch = Scratch::new();
        let mut config = config_in(&scratch, &[]);
        let script = config.lossless_wrapperctl.display().to_string();
        assert_eq!(config.helper_words(&["status"]), ["python3", "-I", script.as_str(), "status"]);
        assert!(!config.helper_present());
        std::fs::create_dir_all(config.lossless_wrapperctl.parent().unwrap()).unwrap();
        std::fs::write(&config.lossless_wrapperctl, "").unwrap();
        assert!(config.helper_present());

        let program = scratch.dir.path().join("amui");
        config.run_helper_as(&program, &["wrapper"]);
        let program_text = program.display().to_string();
        assert_eq!(config.helper_words(&["login", "--pipe"]), [program_text.as_str(), "wrapper", "login", "--pipe"]);
        assert!(!config.helper_present(), "the program is not there");
        std::fs::write(&program, "").unwrap();
        assert!(config.helper_present());
        let command = config.helper_command(&["stop"]);
        assert_eq!(command.get_program(), program.as_os_str());
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["wrapper", "stop"]);
    }

    #[test]
    fn the_desktop_s_language_is_a_language_tag() {
        assert_eq!(language_tag("en_US.UTF-8").as_deref(), Some("en-US"));
        assert_eq!(language_tag("de_DE@euro").as_deref(), Some("de-DE"));
        assert_eq!(language_tag("de").as_deref(), Some("de"));
        assert_eq!(language_tag("zh_Hant_TW").as_deref(), Some("zh-Hant-TW"));
        assert_eq!(language_tag("pt-br").as_deref(), Some("pt-BR"));
        for none in ["C", "POSIX", "C.UTF-8", "", "en_", "english_US", "en_U$"] {
            assert_eq!(language_tag(none), None, "{none}");
        }
        let scratch = Scratch::new();
        assert_eq!(config_in(&scratch, &[]).language, None);
        assert_eq!(config_in(&scratch, &[("LANG", "en_GB.UTF-8")]).language.as_deref(), Some("en-GB"));
        // the more specific setting first, and one that names no language is passed over
        let config = config_in(&scratch, &[("LANG", "en_GB.UTF-8"), ("LC_MESSAGES", "de_DE.UTF-8"), ("LC_ALL", "C")]);
        assert_eq!(config.language.as_deref(), Some("de-DE"));
        let config = config_in(&scratch, &[("LANG", "en_GB.UTF-8"), ("AMUI_LANGUAGE", "ja_JP")]);
        assert_eq!(config.language.as_deref(), Some("ja-JP"));
    }

    #[test]
    fn renderer_options_from_the_environment_win() {
        let scratch = Scratch::new();
        let config = config_in(&scratch, &[("CAVERN_LAB_OPTIONS", "--surround-phase 1")]);
        assert_eq!(config.lab_render_options(), "--surround-phase 1");
        config.set_bass_manage(true);
        assert_eq!(config.lab_render_options(), "--surround-phase 1");
        // an empty value is an override too: no switches
        let config = config_in(&scratch, &[("CAVERN_LAB_OPTIONS", "")]);
        assert_eq!(config.lab_render_options(), "");
    }

    #[test]
    fn bass_management_changes_the_render_generation() {
        let scratch = Scratch::new();
        let config = config_in(&scratch, &[]);
        assert_eq!(config.render_script, config.lab_render_script);
        assert!(!config.bass_manage());
        assert_eq!(config.render_generation(), "cavern-lab-2");
        assert_eq!(config.lab_render_options(), LAB_FIXES);
        config.set_bass_manage(true);
        assert_eq!(config.render_generation(), "cavern-lab-2+bm");
        assert_eq!(config.lab_render_options(), format!("{LAB_FIXES} {LAB_BASS_FEED}"));

        let custom = config_in(&scratch, &[("AMUI_RENDER_SCRIPT", "/opt/render.sh")]);
        assert_eq!(custom.render_script, PathBuf::from("/opt/render.sh"));
        assert_eq!(custom.render_generation(), "custom"); // only the lab renderer has the feed
    }

    #[test]
    fn runtime_folders_are_private() {
        let scratch = Scratch::new();
        let config = config_in(&scratch, &[]);
        config.ensure_dirs().unwrap();
        config.ensure_dirs().unwrap();
        for path in
            [&config.var, &config.var.join("cache"), &config.media_cache_dir, &config.state_dir, &config.tmp_dir]
        {
            assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o700, "{path:?}");
        }
        let problems = config.workspace_ready();
        assert!(problems.iter().any(|p| p.contains("DRM helper not built")));
        assert!(problems.iter().any(|p| p.contains("Atmos renderer not built")));
        assert!(!config.native_signin_built());
    }
}
