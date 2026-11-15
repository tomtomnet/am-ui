//! `amui doctor`: what this machine has and lacks to run am-ui.
//!
//! The report of the Python module (`amui/doctor.py`), section by section, with a hint per
//! missing item: the programs the engine starts, what it uses of the workspace folder
//! (`config.workspace`: the bundled `workspace/` unless `AMUI_WORKSPACE` or an existing private
//! one takes over), the renderer in use, the 9.1.6 PipeWire sink, the private data folder, the
//! sign-in (state only, never the token) and the lossless route. Nothing secret is printed. The
//! exit status is 1 when something required is missing; `./setup.sh` is the fix for nearly every
//! line.
//!
//! The doctor looks at files of the configuration itself. Everything else it asks of the machine
//! goes through a [`Machine`]: where a program is, what a command prints, what a variable of the
//! environment holds. [`Host`] is the real machine; tests answer themselves and so never look
//! for or run a real program.
//!
//! [`examine`] makes the [`Report`]; [`run`] prints it with its verdict and returns the exit
//! status (Python's `cmd_doctor`).
//!
//! Differences from `amui/doctor.py`:
//!
//! * Rows that describe the Python engine are left out:
//!   * the section "Python": `python` (the interpreter's version), `PySide6`, `numpy` and
//!     `environment` (the private Python environment);
//!   * "Programs": the browser (`browser for sign-in`, `browser (alternative sign-in)`), because
//!     this engine has no browser sign-in;
//!   * "Engine tools": `tools/music-web-probe.py`, `tools/music_session.py`,
//!     `tools/music-web-login.py`, `tools/music_browser.py` and `tools/downmix.py`. The Python
//!     engine loaded these from the workspace; what they did is built into this one (`web`,
//!     `session`, `downmix`). The DRM helper and the saved browser session stay;
//!   * "Engine": `amui.web / amui.session import`.
//! * "what Sign In does here" has no browser sign-in to name. Where the native sign-in cannot
//!   run, the row is `absent` and says so, where Python's said "browser sign-in (no lossless:
//!   ...)"; where its component cannot start, the row ends after "(see the lossless line)".
//!   The row of the saved browser session says whose sign-in creates that folder.
//! * The lossless row is made from the two file checks of [`crate::lossless`] (built, signed
//!   in). Python took them from `lossless.status()`, which also asks the service on loopback
//!   for an answer the doctor does not use.
//! * The two questions for the lossless route's control script (its build state, whether the
//!   wrapper can start) are still `python3 -I <config.lossless_wrapperctl> build-state` and
//!   `selftest`, as in Python: the script is ported separately. [`wrapperctl`] is the one place
//!   that spells that command line, for this module and for `lossless --stop` of the command
//!   line. `python3` itself is not a row (it was the interpreter the Python doctor ran in).
//! * A check cannot fail half-way here, so there are no rows "check failed: ..." and "status
//!   unavailable (...)".
//! * `var/ writable` names the system's error as `PermissionError: [Errno 13] Permission denied`
//!   without the path Python's text ended with.
//! * Output of a program that is not UTF-8 is read with replacement characters (Python: the
//!   check failed).

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{self, Config};
use crate::downmix::{Output, Programs, RunError, Runner, py_splitlines, py_strip};
use crate::lossless;
use crate::render::os_error_name;
use crate::session::{self, Route, State};
use crate::util;
use crate::web::{Source, Web};

/// The downmix sink's `node.name`.
pub const SINK_NAME: &str = config::SINK_NAME;

// --- the machine --------------------------------------------------------------------

/// The machine as the doctor asks about it.
pub trait Machine: Send + Sync {
    /// Where a program is on the search path, as a shell would find it (Python's
    /// `shutil.which`); `None` when there is none.
    fn which(&self, program: &str) -> Option<PathBuf>;

    /// Run a command (the program first) to its end, without input, and collect what it
    /// printed. `timeout` `None` waits without limit.
    fn run(&self, argv: &[&str], timeout: Option<Duration>) -> Result<Output, RunError>;

    /// A variable of the environment; `None` when it is not set.
    fn variable(&self, name: &str) -> Option<String>;
}

/// This machine: real programs, found along the `PATH` of an environment.
#[derive(Debug, Clone, Default)]
pub struct Host {
    variables: HashMap<String, String>,
}

impl Host {
    /// The machine as an environment describes it (`PATH`, `HOME`, `AMUI_DOTNET`).
    pub fn new(env: &HashMap<String, String>) -> Host {
        Host { variables: env.clone() }
    }

    /// The machine as the environment of this process describes it: the doctor's one look at
    /// the environment, as `Config::from_process()` is the configuration's.
    pub fn from_process() -> Host {
        let lossy = |text: std::ffi::OsString| text.to_string_lossy().into_owned();
        Host { variables: std::env::vars_os().map(|(name, value)| (lossy(name), lossy(value))).collect() }
    }
}

/// Whether the user of this process may run the file.
fn executable(path: &Path) -> bool {
    let Ok(name) = CString::new(path.as_os_str().as_bytes()) else { return false };
    // SAFETY: `name` is a valid C string for the duration of the call.
    unsafe { libc::access(name.as_ptr(), libc::X_OK) == 0 }
}

/// A file that can be run.
fn usable(path: &Path) -> bool {
    path.is_file() && executable(path)
}

impl Machine for Host {
    fn which(&self, program: &str) -> Option<PathBuf> {
        if program.contains('/') {
            return Some(PathBuf::from(program)).filter(|path| usable(path));
        }
        let search = self.variables.get("PATH").map_or("/bin:/usr/bin", String::as_str);
        if search.is_empty() {
            return None;
        }
        std::env::split_paths(search).map(|folder| folder.join(program)).find(|candidate| usable(candidate))
    }

    fn run(&self, argv: &[&str], timeout: Option<Duration>) -> Result<Output, RunError> {
        Programs.run(argv, timeout)
    }

    fn variable(&self, name: &str) -> Option<String> {
        self.variables.get(name).cloned()
    }
}

/// Everything a command printed, its error output after its output; `None` when it could not be
/// started or ran out of time (Python's `_run`).
fn printed(machine: &dyn Machine, argv: &[&str], timeout_s: u64) -> Option<String> {
    let done = machine.run(argv, Some(Duration::from_secs(timeout_s))).ok()?;
    Some(done.stdout + &done.stderr)
}

// --- the lossless route's control script ----------------------------------------------

/// Run a command of the lossless route's control helper (`Config::helper_words`): the program
/// of am-ui that runs itself in that role, or the Python script.
pub fn wrapperctl(
    config: &Config,
    machine: &dyn Machine,
    command: &str,
    timeout: Option<Duration>,
) -> Result<Output, RunError> {
    let words = config.helper_words(&[command]);
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    machine.run(&words, timeout)
}

/// `absent`, `stale`, `unstaged` or `current`, as the control script sees the build of the
/// lossless route; empty when it could not be asked.
fn lossless_build_state(config: &Config, machine: &dyn Machine) -> String {
    match wrapperctl(config, machine, "build-state", Some(Duration::from_secs(15))) {
        Ok(done) if done.status == Some(0) => done.stdout.split_whitespace().next().unwrap_or("").to_string(),
        _ => String::new(),
    }
}

/// Why Apple's component (the wrapper) cannot start on this system; empty when it starts or when
/// the check could not be made. The check needs no account and contacts nobody: the wrapper is
/// asked for its usage text, in the private namespace it always runs in.
fn lossless_start_problem(config: &Config, machine: &dyn Machine) -> String {
    let Ok(done) = wrapperctl(config, machine, "selftest", Some(Duration::from_secs(45))) else {
        return String::new();
    };
    if done.status == Some(0) {
        return String::new();
    }
    let lines = py_splitlines(&done.stderr);
    let last = lines.iter().map(|line| py_strip(line)).rfind(|line| !line.is_empty());
    let last = last.unwrap_or("the start check failed");
    last.strip_prefix("error: ").unwrap_or(last).to_string()
}

// --- the report -------------------------------------------------------------------------

/// What the doctor found of an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    Ok,
    /// Not there, and required.
    Missing,
    /// Not there; the engine runs without it.
    Absent,
}

impl Found {
    pub fn as_str(&self) -> &'static str {
        match self {
            Found::Ok => "ok",
            Found::Missing => "MISSING",
            Found::Absent => "absent",
        }
    }

    fn of(present: bool) -> Found {
        if present { Found::Ok } else { Found::Missing }
    }
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// The title of a section.
    Section(String),
    Item {
        found: Found,
        item: String,
        detail: String,
    },
}

/// What the doctor found, in the order it looked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub rows: Vec<Row>,
    /// How many required items are missing.
    pub missing_required: usize,
}

impl Report {
    /// Add an item. A missing one counts when it is `required`, and is `absent` when not.
    pub fn add(&mut self, found: Found, item: impl Into<String>, detail: impl Into<String>, required: bool) {
        let found = match found {
            Found::Missing if required => {
                self.missing_required += 1;
                Found::Missing
            }
            Found::Missing => Found::Absent,
            other => other,
        };
        self.rows.push(Row::Item { found, item: item.into(), detail: detail.into() });
    }

    pub fn section(&mut self, title: impl Into<String>) {
        self.rows.push(Row::Section(title.into()));
    }

    /// The rows as they are printed: an empty line and the title for a section, status, item and
    /// detail in columns for an item.
    pub fn text(&self) -> String {
        let mut text = String::new();
        for row in &self.rows {
            match row {
                Row::Section(title) => text.push_str(&format!("\n{title}\n")),
                Row::Item { found, item, detail } => {
                    text.push_str(&format!("  {:<8} {item:<34} {detail}\n", found.as_str()));
                }
            }
        }
        text
    }

    /// The last line of the doctor.
    pub fn verdict(&self) -> String {
        match self.missing_required {
            0 => "Everything required is in place.".to_string(),
            missing => format!("{missing} required item(s) missing. Run ./setup.sh (README.md: Setup)."),
        }
    }

    /// The doctor's exit status: 1 when something required is missing.
    pub fn status(&self) -> i32 {
        if self.missing_required == 0 { 0 } else { 1 }
    }
}

// --- the checks -------------------------------------------------------------------------

/// The dotnet the renderer will use and where it is from: same order as
/// tools/cavern-lab/build-env.sh.
fn dotnet(config: &Config, machine: &dyn Machine) -> Option<(PathBuf, &'static str)> {
    let named = machine.variable("AMUI_DOTNET").map(|named| named.trim().to_string()).unwrap_or_default();
    if !named.is_empty() {
        let home = machine.variable("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
        let named = util::expand_user(&named, &home);
        if usable(&named) {
            return Some((named, "AMUI_DOTNET"));
        }
    }
    let private = config.var.join("toolchains/dotnet/dotnet");
    if usable(&private) {
        return Some((private, "private SDK in var/toolchains"));
    }
    for workspace in [&config.workspace, &config.legacy_workspace] {
        let hint = workspace.join("builds/music-downloader/dotnet-path.txt");
        if let Ok(text) = fs::read_to_string(hint) {
            let path = PathBuf::from(py_strip(&text));
            if usable(&path) {
                return Some((path, "workspace SDK"));
            }
        }
    }
    machine.which("dotnet").map(|found| (found, "PATH"))
}

fn check_programs(report: &mut Report, config: &Config, machine: &dyn Machine) {
    report.section("Programs");
    match machine.which("ffmpeg") {
        Some(ffmpeg) => {
            let buildconf = printed(machine, &["ffmpeg", "-hide_banner", "-buildconf"], 10).unwrap_or_default();
            let decoders = printed(machine, &["ffmpeg", "-hide_banner", "-decoders"], 10).unwrap_or_default();
            let mut notes = Vec::new();
            if !buildconf.contains("--enable-libsoxr") {
                notes.push("no libsoxr (resampling falls back to swr)");
            }
            if !decoders.contains(" alac ") {
                notes.push("no ALAC decoder (lossless playback needs it)");
            }
            if !decoders.contains(" aac ") && !decoders.contains(" libfdk_aac ") {
                notes.push("no AAC decoder (stereo playback needs it)");
            }
            let notes = if notes.is_empty() { String::new() } else { format!(" — {}", notes.join("; ")) };
            report.add(Found::Ok, "ffmpeg", format!("{}{notes}", ffmpeg.display()), true);
        }
        None => report.add(Found::Missing, "ffmpeg", "install ffmpeg (decodes AAC/ALAC, measures loudness)", true),
    }
    match machine.which("ffprobe") {
        Some(ffprobe) => report.add(Found::Ok, "ffprobe", ffprobe.display().to_string(), true),
        None => report.add(Found::Missing, "ffprobe", "comes with ffmpeg", true),
    }
    // the older application's sink only: am-ui plays and downmixes on its own
    match machine.which("pw-cli") {
        Some(found) => report.add(Found::Ok, "pw-cli", found.display().to_string(), false),
        None => report.add(Found::Missing, "pw-cli", "only for `amui downmix` on the older application's sink", false),
    }
    match dotnet(config, machine) {
        Some((dotnet, source)) => report.add(Found::Ok, "dotnet", format!("{} ({source})", dotnet.display()), true),
        None => report.add(
            Found::Missing,
            "dotnet",
            "runs (and builds) the patched Cavern renderer: ./setup.sh installs a private SDK",
            true,
        ),
    }
}

/// Empty when the DRM helper in use is this repository's build of the current sources; else
/// what to do.
///
/// Asks `tools/drm-helper/build.sh --check` (it compares a record of what the helper was built
/// from; it changes nothing).
pub fn drm_helper_note(config: &Config, machine: &dyn Machine) -> String {
    let waiting = "an update is waiting (run ./setup.sh): songs Apple offers only as AAC need the newer helper";
    if config.drm_helper != config.own_drm_helper {
        return format!("the workspace's own build; {waiting}");
    }
    let script = config.root.join("tools/drm-helper/build.sh");
    match machine.run(&[script.to_string_lossy().as_ref(), "--check"], Some(Duration::from_secs(20))) {
        Ok(done) if done.status == Some(2) => waiting.to_string(),
        _ => String::new(),
    }
}

fn check_workspace(report: &mut Report, config: &Config, machine: &dyn Machine) {
    report.section(format!("Engine tools ({}: {})", config.workspace_source, config.workspace.display()));
    if !config.workspace.is_dir() {
        let hint = "unset AMUI_WORKSPACE to use the workspace/ folder of this repository";
        report.add(Found::Missing, "workspace folder", hint, true);
    }
    let helper = &config.drm_helper;
    // the helper may be this repository's build, not the workspace's
    let bases = [&config.workspace, &config.root];
    let name = bases.iter().find_map(|base| helper.strip_prefix(base).ok()).unwrap_or(helper.as_path());
    let (present, detail) = if !helper.is_file() {
        (false, "Widevine license/decrypt helper, built by ./setup.sh (tools/drm-helper/build.sh)".to_string())
    } else if !executable(helper) {
        (false, "present but not executable (chmod +x)".to_string())
    } else {
        (true, drm_helper_note(config, machine))
    };
    report.add(Found::of(present), name.display().to_string(), detail, true);
    let saved = config.web_session_dir.is_dir();
    let detail = if saved { "" } else { "created by the browser sign-in of the Python application" };
    report.add(if saved { Found::Ok } else { Found::Absent }, "builds/music-downloader/web-session/", detail, false);
}

fn check_renderer(report: &mut Report, config: &Config) {
    report.section("Renderer");
    let script = &config.render_script;
    let fallback = script.is_file() && *script != config.lab_render_script;
    if config.lab_probe_dll.is_file() {
        let generation = Some(config.render_generation()).filter(|generation| !generation.is_empty());
        let generation = generation.unwrap_or_else(|| "cavern-lab".to_string());
        report.add(Found::Ok, "patched Cavern (tools/cavern-lab)", format!("generation {generation}"), true);
    } else {
        let until = if fallback { "; until then the workspace script renders" } else { "" };
        let hint = format!("not built: ./setup.sh (or tools/cavern-lab/build.sh){until}");
        report.add(Found::Missing, "patched Cavern (tools/cavern-lab)", hint, !fallback);
    }
    report.add(Found::of(script.is_file()), "render script in use", script.display().to_string(), true);
}

/// Where a PulseAudio client finds the session's sound server (pipewire-pulse).
enum SoundServer {
    /// A local socket: `PULSE_SERVER=unix:…`, else `$XDG_RUNTIME_DIR/pulse/native`.
    Socket(PathBuf),
    /// `PULSE_SERVER` names a server elsewhere.
    Named(String),
    /// Neither variable is set: no desktop session to look in.
    Unknown,
}

fn sound_server(machine: &dyn Machine) -> SoundServer {
    if let Some(server) = machine.variable("PULSE_SERVER").filter(|server| !server.is_empty()) {
        return match server.strip_prefix("unix:") {
            Some(socket) => SoundServer::Socket(PathBuf::from(socket)),
            None => SoundServer::Named(server),
        };
    }
    match machine.variable("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => SoundServer::Socket(Path::new(&dir).join("pulse/native")),
        None => SoundServer::Unknown,
    }
}

fn check_sink(report: &mut Report, machine: &dyn Machine) {
    report.section("Sound");
    // am-ui plays a stereo stream of its own through the session's sound server
    let server = "sound server";
    match sound_server(machine) {
        SoundServer::Socket(socket) if socket.exists() => {
            report.add(Found::Ok, server, socket.display().to_string(), true)
        }
        SoundServer::Socket(socket) => {
            let hint = format!("{} is not there: is pipewire-pulse (or PulseAudio) running?", socket.display());
            report.add(Found::Missing, server, hint, true);
        }
        SoundServer::Named(name) => report.add(Found::Ok, server, format!("{name} (PULSE_SERVER)"), true),
        SoundServer::Unknown => {
            report.add(Found::Missing, server, "no XDG_RUNTIME_DIR: run this in the desktop session", true);
        }
    }
    // the older application's sink: no longer needed by am-ui, reported for that application
    if machine.which("pw-cli").is_none() {
        return;
    }
    match printed(machine, &["pw-cli", "ls", "Node"], 15) {
        Some(nodes) if nodes.contains(SINK_NAME) => {
            report.add(
                Found::Ok,
                SINK_NAME,
                "the older application's downmix sink is running (am-ui does not need it)",
                false,
            );
        }
        _ => report.add(Found::Missing, SINK_NAME, "not running (am-ui downmixes on its own)", false),
    }
}

/// The system's error as Python names it: `PermissionError: [Errno 13] Permission denied`.
fn os_error_text(error: &io::Error) -> String {
    let said = error.to_string();
    match error.raw_os_error() {
        Some(number) => {
            let said = said.strip_suffix(&format!(" (os error {number})")).unwrap_or(&said);
            format!("{}: [Errno {number}] {said}", os_error_name(error))
        }
        None => format!("{}: {said}", os_error_name(error)),
    }
}

fn check_var(report: &mut Report, config: &Config) {
    report.section(format!("Private runtime data (AMUI_VAR = {})", config.var.display()));
    let probe = config.var.join(".doctor-write-test");
    let written = config.ensure_dirs().and_then(|()| {
        let mut file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&probe)?;
        file.write_all(b"ok\n")?;
        fs::remove_file(&probe)
    });
    match written {
        Ok(()) => report.add(Found::Ok, "var/ writable", "", true),
        Err(error) => report.add(Found::Missing, "var/ writable", os_error_text(&error), true),
    }
}

fn check_engine(report: &mut Report, web: &Web, machine: &dyn Machine) {
    let config = web.config();
    report.section("Engine");
    let build_state = if config.native_signin_built() { lossless_build_state(config, machine) } else { String::new() };
    let start_problem = if matches!(build_state.as_str(), "current" | "stale") {
        lossless_start_problem(config, machine)
    } else {
        String::new()
    };

    let status = session::status(web);
    let connected = status.state == State::Connected;
    let mut detail = match status.state {
        State::Connected => "connected",
        State::Unknown => "saved, not verified yet (./am-ui cli check; the app checks it when it starts)",
        State::ReconnectRequired => "rejected by Apple — sign in again with the app's Sign In",
        State::NoSession => "none — press Sign In in the app",
    }
    .to_string();
    if let Some(signin) = status.signin {
        let whose = if signin == Source::Android { "Apple's native sign-in" } else { "browser session" };
        detail = format!("{whose}: {detail}");
    }
    if let Some(until) = status.cookie_expires_at.as_deref().filter(|until| connected && !until.is_empty()) {
        detail.push_str(&format!(", valid until {until}"));
    }
    report.add(if connected { Found::Ok } else { Found::Absent }, "Apple Music sign-in", detail, false);
    match (status.signin_route, start_problem.is_empty()) {
        (Route::Android, true) => {
            report.add(Found::Ok, "what Sign In does here", "Apple's native sign-in (lossless available)", true);
        }
        (Route::Android, false) => report.add(
            Found::Absent,
            "what Sign In does here",
            "Apple's native sign-in, whose component cannot start on this computer (see the lossless line)",
            true,
        ),
        (Route::None, _) => report.add(
            Found::Absent,
            "what Sign In does here",
            "nothing: Apple's native sign-in needs the lossless route, which is not built",
            true,
        ),
    }

    let (built, signed) = (lossless::helper_built(config), lossless::signed_in_files(config));
    let mut detail = match (built, signed) {
        (true, true) => "built and signed in".to_string(),
        (true, false) => "built, not signed in: press Sign In in the app".to_string(),
        (false, _) => {
            "not built: ./setup.sh builds it on x86_64 (it was run with --no-lossless, or the build failed)".to_string()
        }
    };
    if built && !start_problem.is_empty() {
        detail = format!("built, but {start_problem}");
    }
    if built && matches!(build_state.as_str(), "stale" | "unstaged") {
        detail.push_str("; an update is waiting (quit am-ui, tools/lossless/stop.sh, then ./setup.sh)");
    }
    let works = built && signed && start_problem.is_empty();
    report.add(if works { Found::Ok } else { Found::Absent }, "lossless (ALAC) route", detail, false);
}

/// Look at the machine: the configuration and the sign-in of `web`, programs through `machine`.
///
/// Creates the private data folders when they are missing (the check that they can be written).
pub fn examine(web: &Web, machine: &dyn Machine) -> Report {
    let config = web.config();
    let mut report = Report::default();
    check_programs(&mut report, config, machine);
    check_workspace(&mut report, config, machine);
    check_renderer(&mut report, config);
    check_sink(&mut report, machine);
    check_var(&mut report, config);
    check_engine(&mut report, web, machine);
    report
}

/// The doctor: print the report and its verdict to `out`; the exit status is 1 when something
/// required is missing.
pub fn run(web: &Web, machine: &dyn Machine, out: &mut dyn Write) -> i32 {
    let report = examine(web, machine);
    // a reader that went away does not change the verdict
    let _ = write!(out, "{}\n{}\n", report.text(), report.verdict());
    report.status()
}

#[cfg(test)]
pub(crate) mod testing {
    //! A stand-in machine for the tests of this crate.
    use super::*;
    use std::sync::Mutex;

    /// One command a [`FakeMachine`] was asked to run, with its time limit.
    pub type Call = (Vec<String>, Option<Duration>);

    /// A machine that has the programs a test gives it and answers commands from a list.
    #[derive(Default)]
    pub struct FakeMachine {
        /// Programs on the search path: name -> where.
        pub programs: Mutex<HashMap<String, PathBuf>>,
        /// What commands leave: a part of the command line (its words joined by spaces) and the
        /// outcome. The first entry that fits answers; a command without one cannot be started.
        pub answers: Mutex<Vec<(String, Result<Output, RunError>)>>,
        pub variables: Mutex<HashMap<String, String>>,
        pub calls: Mutex<Vec<Call>>,
    }

    impl FakeMachine {
        /// A machine without programs.
        pub fn new() -> FakeMachine {
            FakeMachine::default()
        }

        /// Put programs on the search path, each as `/usr/bin/<name>`.
        pub fn install(&self, names: &[&str]) {
            let mut programs = self.programs.lock().unwrap();
            for name in names {
                programs.insert(name.to_string(), PathBuf::from(format!("/usr/bin/{name}")));
            }
        }

        /// Let commands whose command line contains `part` end with `status`, having printed
        /// `stdout` and `stderr`. A later call for the same part replaces the earlier one.
        pub fn answer(&self, part: &str, status: i32, stdout: &str, stderr: &str) {
            let output = Output { status: Some(status), stdout: stdout.to_string(), stderr: stderr.to_string() };
            self.outcome(part, Ok(output));
        }

        pub fn outcome(&self, part: &str, outcome: Result<Output, RunError>) {
            let mut answers = self.answers.lock().unwrap();
            answers.retain(|(known, _)| known != part);
            answers.push((part.to_string(), outcome));
        }

        pub fn set_variable(&self, name: &str, value: &str) {
            self.variables.lock().unwrap().insert(name.to_string(), value.to_string());
        }

        /// The command lines run so far, words joined by spaces.
        pub fn command_lines(&self) -> Vec<String> {
            self.calls.lock().unwrap().iter().map(|(argv, _)| argv.join(" ")).collect()
        }
    }

    impl Machine for FakeMachine {
        fn which(&self, program: &str) -> Option<PathBuf> {
            self.programs.lock().unwrap().get(program).cloned()
        }

        fn run(&self, argv: &[&str], timeout: Option<Duration>) -> Result<Output, RunError> {
            self.calls.lock().unwrap().push((argv.iter().map(|word| word.to_string()).collect(), timeout));
            let line = argv.join(" ");
            let answers = self.answers.lock().unwrap();
            answers
                .iter()
                .find(|(part, _)| line.contains(part))
                .map_or(Err(RunError::NotFound), |(_, outcome)| outcome.clone())
        }

        fn variable(&self, name: &str) -> Option<String> {
            self.variables.lock().unwrap().get(name).cloned()
        }
    }
}

#[cfg(test)]
mod tests {
    //! The doctor on scratch configurations and a stand-in machine: no real program is looked for
    //! or run, no real sink, no network (the sign-in state is read from files).
    use super::testing::FakeMachine;
    use super::*;
    use crate::acquire::testing::write_program;
    use crate::web::testing::{ANDROID_TOKEN, Answer, BROWSER_TOKEN, FakeTransport, sign_in};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    const HELPER: &str = "builds/music-downloader/amdl-linux-target/release/amdl-linux-probe";
    const SELFTEST_FAILURE: &str = "Apple's component for the sign-in and for lossless cannot start on this computer: \
                                    this system did not let it set up the private namespace it runs in \
                                    (unshare: Operation not permitted)";

    /// A scratch am-ui folder, a network nothing may use, and a machine without programs.
    struct Scratch {
        dir: tempfile::TempDir,
        config: Arc<Config>,
        web: Web,
        machine: FakeMachine,
    }

    impl Scratch {
        fn new() -> Scratch {
            Scratch::with(&[], |_| {})
        }

        /// `prepare` gets the am-ui folder before the configuration is made over it.
        fn with(env: &[(&str, &str)], prepare: impl FnOnce(&Path)) -> Scratch {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("am-ui");
            fs::create_dir_all(root.join("workspace")).unwrap();
            prepare(&root);
            let env = env.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect();
            let config = Arc::new(Config::new(root, &dir.path().join("home"), &env));
            let (transport, _seen) =
                FakeTransport::new(|_| -> Result<Answer, _> { panic!("the doctor asked the network") });
            let web = Web::with_transport(config.clone(), Box::new(transport));
            Scratch { dir, config, web, machine: FakeMachine::new() }
        }

        fn report(&self) -> Report {
            examine(&self.web, &self.machine)
        }

        /// What the doctor prints, and its exit status.
        fn doctor(&self) -> (String, i32) {
            let mut out = Vec::new();
            let status = run(&self.web, &self.machine, &mut out);
            (String::from_utf8(out).unwrap(), status)
        }

        /// The row of an item: (status, detail).
        fn row(&self, item: &str) -> (&'static str, String) {
            row(&self.report(), item)
        }

        /// The path under the scratch folder as the report prints it.
        fn shown(&self, path: &Path) -> String {
            path.display().to_string()
        }
    }

    fn row(report: &Report, wanted: &str) -> (&'static str, String) {
        let found = report.rows.iter().find_map(|row| match row {
            Row::Item { found, item, detail } if item == wanted => Some((found.as_str(), detail.clone())),
            _ => None,
        });
        found.unwrap_or_else(|| panic!("no row {wanted}: {report:?}"))
    }

    fn write(path: &Path, text: &str, mode: u32) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// This repository's build of the DRM helper and of the patched renderer, with its script.
    fn build_engine(root: &Path) {
        write(&root.join("workspace").join(HELPER), "stand-in\n", 0o755);
        write(&root.join("tools/cavern-lab/probe/bin/Release/net8.0/CavernProbe.dll"), "stand-in\n", 0o644);
        write(&root.join("tools/cavern-lab/render-music-916-lab.sh"), "stand-in\n", 0o755);
    }

    /// A built and staged lossless route, signed in or not.
    fn build_lossless(config: &Config, signed_in: bool) {
        let runtime = config.lossless_data_dir.parent().unwrap();
        for path in [
            config.lossless_wrapperctl.clone(),
            config.lossless_rip_helper.clone(),
            config.lossless_build_dir.join("bin/wrapper-lite-rootless"),
            runtime.join("system/bin/lite"),
        ] {
            write(&path, "stand-in\n", 0o755);
        }
        util::create_private_dirs(&config.lossless_data_dir).unwrap();
        if signed_in {
            sign_in(config, Source::Android, ANDROID_TOKEN);
            for name in ["STOREFRONT_ID", "DEV_TOKEN"] {
                write(&config.lossless_data_dir.join(name), "stand-in\n", 0o600);
            }
        }
    }

    /// Every program the doctor looks for, a running sink, a current helper and a wrapper that
    /// starts.
    fn equip(machine: &FakeMachine, runtime: &Path) {
        // the session's sound server
        write(&runtime.join("pulse/native"), "", 0o600);
        machine.set_variable("XDG_RUNTIME_DIR", &runtime.display().to_string());
        machine.install(&["ffmpeg", "ffprobe", "pw-cli", "dotnet"]);
        machine.answer("-buildconf", 0, "  configuration:\n    --enable-gpl\n    --enable-libsoxr\n", "");
        machine.answer(
            "-decoders",
            0,
            " A....D aac                  AAC (Advanced Audio Coding)\n A....D alac                 ALAC\n",
            "",
        );
        machine.answer(
            "pw-cli ls Node",
            0,
            &format!("\tid 38, type PipeWire:Interface:Node/3\n \t\tnode.name = \"{SINK_NAME}\"\n"),
            "",
        );
        machine.answer("build.sh --check", 0, "", "");
        machine.answer("build-state", 0, "current running\n", "");
        machine.answer("selftest", 0, "ok: the wrapper starts on this system\n", "");
    }

    #[test]
    fn everything_in_place() {
        let scratch = Scratch::with(&[], build_engine);
        let config = &scratch.config;
        build_lossless(config, true);
        let checked = r#"{"state": "connected", "checked_at": "2026-01-02T03:04:05+00:00"}"#;
        write(&config.android_session_status_file, checked, 0o600);
        fs::create_dir_all(&config.web_session_dir).unwrap();
        equip(&scratch.machine, &scratch.dir.path().join("runtime"));
        let (text, status) = scratch.doctor();
        let (workspace, var) = (scratch.shown(&config.workspace), scratch.shown(&config.var));
        let script = scratch.shown(&config.lab_render_script);
        let sound = scratch.shown(&scratch.dir.path().join("runtime/pulse/native"));
        let expected = format!(
            "\n\
             Programs\n\
             \x20 ok       ffmpeg                             /usr/bin/ffmpeg\n\
             \x20 ok       ffprobe                            /usr/bin/ffprobe\n\
             \x20 ok       pw-cli                             /usr/bin/pw-cli\n\
             \x20 ok       dotnet                             /usr/bin/dotnet (PATH)\n\
             \n\
             Engine tools (bundled: {workspace})\n\
             \x20 ok       {HELPER} \n\
             \x20 ok       builds/music-downloader/web-session/ \n\
             \n\
             Renderer\n\
             \x20 ok       patched Cavern (tools/cavern-lab)  generation cavern-lab-2\n\
             \x20 ok       render script in use               {script}\n\
             \n\
             Sound\n\
             \x20 ok       sound server                       {sound}\n\
             \x20 ok       atmos_916_downmix                  the older application's downmix sink is running (am-ui does not need it)\n\
             \n\
             Private runtime data (AMUI_VAR = {var})\n\
             \x20 ok       var/ writable                      \n\
             \n\
             Engine\n\
             \x20 ok       Apple Music sign-in                Apple's native sign-in: connected\n\
             \x20 ok       what Sign In does here             Apple's native sign-in (lossless available)\n\
             \x20 ok       lossless (ALAC) route              built and signed in\n\
             \n\
             Everything required is in place.\n"
        );
        assert_eq!(text, expected);
        assert_eq!(status, 0);
        assert!(!text.contains(ANDROID_TOKEN));
        // what was asked of the machine, with Python's time limits
        let calls = scratch.machine.calls.lock().unwrap().clone();
        let helper = scratch.shown(&config.lossless_wrapperctl);
        let asked: Vec<(String, Option<u64>)> =
            calls.iter().map(|(argv, limit)| (argv.join(" "), limit.map(|limit| limit.as_secs()))).collect();
        assert_eq!(
            asked,
            [
                ("ffmpeg -hide_banner -buildconf".to_string(), Some(10)),
                ("ffmpeg -hide_banner -decoders".to_string(), Some(10)),
                (format!("{}/tools/drm-helper/build.sh --check", scratch.shown(&config.root)), Some(20)),
                ("pw-cli ls Node".to_string(), Some(15)),
                (format!("python3 -I {helper} build-state"), Some(15)),
                (format!("python3 -I {helper} selftest"), Some(45)),
            ]
        );
        // the probe file is gone again
        assert!(!config.var.join(".doctor-write-test").exists());
    }

    #[test]
    fn a_machine_that_has_nothing() {
        let scratch = Scratch::new();
        let config = &scratch.config;
        let (text, status) = scratch.doctor();
        let (workspace, var) = (scratch.shown(&config.workspace), scratch.shown(&config.var));
        let script = scratch.shown(&config.lab_render_script);
        let expected = format!(
            "\n\
             Programs\n\
             \x20 MISSING  ffmpeg                             install ffmpeg (decodes AAC/ALAC, measures loudness)\n\
             \x20 MISSING  ffprobe                            comes with ffmpeg\n\
             \x20 absent   pw-cli                             only for `amui downmix` on the older application's sink\n\
             \x20 MISSING  dotnet                             runs (and builds) the patched Cavern renderer: ./setup.sh installs a private SDK\n\
             \n\
             Engine tools (bundled: {workspace})\n\
             \x20 MISSING  {HELPER} Widevine license/decrypt helper, built by ./setup.sh (tools/drm-helper/build.sh)\n\
             \x20 absent   builds/music-downloader/web-session/ created by the browser sign-in of the Python application\n\
             \n\
             Renderer\n\
             \x20 MISSING  patched Cavern (tools/cavern-lab)  not built: ./setup.sh (or tools/cavern-lab/build.sh)\n\
             \x20 MISSING  render script in use               {script}\n\
             \n\
             Sound\n\
             \x20 MISSING  sound server                       no XDG_RUNTIME_DIR: run this in the desktop session\n\
             \n\
             Private runtime data (AMUI_VAR = {var})\n\
             \x20 ok       var/ writable                      \n\
             \n\
             Engine\n\
             \x20 absent   Apple Music sign-in                none — press Sign In in the app\n\
             \x20 absent   what Sign In does here             nothing: Apple's native sign-in needs the lossless route, which is not built\n\
             \x20 absent   lossless (ALAC) route              not built: ./setup.sh builds it on x86_64 (it was run with --no-lossless, or the build failed)\n\
             \n\
             7 required item(s) missing. Run ./setup.sh (README.md: Setup).\n"
        );
        assert_eq!(text, expected);
        assert_eq!(status, 1);
        assert!(scratch.machine.calls.lock().unwrap().is_empty()); // nothing to run
        assert!(config.state_dir.is_dir()); // the data folders were made to see that they can be
    }

    #[test]
    fn rows_count_as_python_counts_them() {
        let mut report = Report::default();
        report.section("A section");
        report.add(Found::Ok, "one", "there", true);
        report.add(Found::Missing, "two", "needed", true);
        report.add(Found::Missing, "three", "nice to have", false);
        report.add(Found::Absent, "four", "", true);
        report.add(Found::Missing, "an item with a name longer than its column", "x", true);
        assert_eq!(report.missing_required, 2);
        assert_eq!(
            report.text(),
            "\nA section\n\
             \x20 ok       one                                there\n\
             \x20 MISSING  two                                needed\n\
             \x20 absent   three                              nice to have\n\
             \x20 absent   four                               \n\
             \x20 MISSING  an item with a name longer than its column x\n"
        );
        assert_eq!(
            (report.status(), report.verdict().as_str()),
            (1, "2 required item(s) missing. Run ./setup.sh (README.md: Setup).")
        );
        let fine = Report::default();
        assert_eq!(
            (fine.status(), fine.verdict().as_str(), fine.text().as_str()),
            (0, "Everything required is in place.", "")
        );
    }

    #[test]
    fn ffmpeg_notes() {
        let scratch = Scratch::new();
        scratch.machine.install(&["ffmpeg"]);
        // neither command answers: every note
        assert_eq!(
            scratch.row("ffmpeg"),
            (
                "ok",
                "/usr/bin/ffmpeg — no libsoxr (resampling falls back to swr); no ALAC decoder (lossless playback \
                 needs it); no AAC decoder (stereo playback needs it)"
                    .to_string()
            )
        );
        scratch.machine.answer("-buildconf", 0, "", "configuration: --enable-libsoxr\n"); // on either stream
        scratch.machine.answer("-decoders", 0, " A....D libfdk_aac           Fraunhofer FDK AAC\n", "");
        assert_eq!(
            scratch.row("ffmpeg"),
            ("ok", "/usr/bin/ffmpeg — no ALAC decoder (lossless playback needs it)".to_string())
        );
        scratch.machine.answer("-decoders", 1, " A....D alac                 ALAC\n", "");
        assert_eq!(
            scratch.row("ffmpeg"),
            ("ok", "/usr/bin/ffmpeg — no AAC decoder (stereo playback needs it)".to_string())
        );
        scratch.machine.outcome("-buildconf", Err(RunError::Timeout));
        scratch.machine.answer(
            "-decoders",
            0,
            " A....D aac                  AAC\n A....D alac                 ALAC\n",
            "",
        );
        assert_eq!(
            scratch.row("ffmpeg"),
            ("ok", "/usr/bin/ffmpeg — no libsoxr (resampling falls back to swr)".to_string())
        );
    }

    #[test]
    fn dotnet_is_looked_for_where_the_renderer_looks() {
        let scratch = Scratch::new();
        let config = &scratch.config;
        let missing = "runs (and builds) the patched Cavern renderer: ./setup.sh installs a private SDK";
        assert_eq!(scratch.row("dotnet"), ("MISSING", missing.to_string()));
        scratch.machine.install(&["dotnet"]);
        assert_eq!(scratch.row("dotnet"), ("ok", "/usr/bin/dotnet (PATH)".to_string()));
        // a path an older workspace noted, then the workspace in use
        let (legacy, bundled) = (scratch.dir.path().join("sdk-a/dotnet"), scratch.dir.path().join("sdk-b/dotnet"));
        write(&legacy, "stand-in\n", 0o755);
        write(
            &config.legacy_workspace.join("builds/music-downloader/dotnet-path.txt"),
            &format!("{}\n", legacy.display()),
            0o644,
        );
        assert_eq!(scratch.row("dotnet"), ("ok", format!("{} (workspace SDK)", legacy.display())));
        write(&bundled, "stand-in\n", 0o755);
        let hint = config.workspace.join("builds/music-downloader/dotnet-path.txt");
        write(&hint, &format!("  {}\n", bundled.display()), 0o644);
        assert_eq!(scratch.row("dotnet"), ("ok", format!("{} (workspace SDK)", bundled.display())));
        // one that cannot be run does not count
        fs::set_permissions(&bundled, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(scratch.row("dotnet"), ("ok", format!("{} (workspace SDK)", legacy.display())));
        fs::write(&hint, "").unwrap();
        assert_eq!(scratch.row("dotnet"), ("ok", format!("{} (workspace SDK)", legacy.display())));
        // the private SDK of ./setup.sh comes before both, a named one before everything
        let private = config.var.join("toolchains/dotnet/dotnet");
        write(&private, "stand-in\n", 0o755);
        assert_eq!(scratch.row("dotnet"), ("ok", format!("{} (private SDK in var/toolchains)", private.display())));
        let named = scratch.dir.path().join("home/sdk/dotnet");
        write(&named, "stand-in\n", 0o755);
        scratch.machine.set_variable("AMUI_DOTNET", &format!(" {} ", named.display()));
        assert_eq!(scratch.row("dotnet"), ("ok", format!("{} (AMUI_DOTNET)", named.display())));
        scratch.machine.set_variable("HOME", &scratch.dir.path().join("home").display().to_string());
        scratch.machine.set_variable("AMUI_DOTNET", "~/sdk/dotnet");
        assert_eq!(scratch.row("dotnet"), ("ok", format!("{} (AMUI_DOTNET)", named.display())));
        scratch.machine.set_variable("AMUI_DOTNET", "/nowhere/dotnet"); // named, but not there: the next one
        assert_eq!(scratch.row("dotnet"), ("ok", format!("{} (private SDK in var/toolchains)", private.display())));
    }

    /// tests/test_config.py: WorkspaceLayout.test_doctor_says_when_the_helper_needs_a_rebuild
    #[test]
    fn doctor_says_when_the_helper_needs_a_rebuild() {
        let scratch = Scratch::with(&[], build_engine);
        let config = &scratch.config;
        assert_eq!(config.drm_helper, config.own_drm_helper);
        scratch.machine.answer("--check", 0, "", "");
        assert_eq!(drm_helper_note(config, &scratch.machine), "");
        scratch.machine.answer("--check", 2, "", "");
        assert!(drm_helper_note(config, &scratch.machine).contains("./setup.sh"));
        let script = config.root.join("tools/drm-helper/build.sh").display().to_string();
        assert_eq!(scratch.machine.calls.lock().unwrap().last().unwrap().0, [script.as_str(), "--check"]);
        scratch.machine.outcome("--check", Err(RunError::NotFound));
        assert_eq!(drm_helper_note(config, &scratch.machine), "");

        // a workspace's own, older build: nothing is asked
        let other = scratch.dir.path().join("another-workspace");
        write(&other.join(config::WORKSPACE_MARKER), "", 0o644);
        write(&other.join(HELPER), "stand-in\n", 0o755);
        let named = Scratch::with(&[("AMUI_WORKSPACE", other.to_str().unwrap())], |_| {});
        assert_ne!(named.config.drm_helper, named.config.own_drm_helper);
        assert!(drm_helper_note(&named.config, &named.machine).contains("./setup.sh"));
        assert!(named.machine.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn the_helper_row() {
        let scratch = Scratch::with(&[], build_engine);
        let config = &scratch.config;
        let waiting = "an update is waiting (run ./setup.sh): songs Apple offers only as AAC need the newer helper";
        scratch.machine.answer("--check", 2, "", "");
        assert_eq!(scratch.row(HELPER), ("ok", waiting.to_string()));
        fs::set_permissions(&config.drm_helper, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(scratch.row(HELPER), ("MISSING", "present but not executable (chmod +x)".to_string()));
        assert_eq!(scratch.report().missing_required, 5); // ffmpeg, ffprobe, dotnet, the helper, the sound server

        // another workspace with its own build: named by its place in that workspace
        let other = scratch.dir.path().join("another-workspace");
        write(&other.join(config::WORKSPACE_MARKER), "", 0o644);
        write(&other.join(HELPER), "stand-in\n", 0o755);
        let named = Scratch::with(&[("AMUI_WORKSPACE", other.to_str().unwrap())], |_| {});
        assert_eq!(named.row(HELPER), ("ok", format!("the workspace's own build; {waiting}")));
        let title = format!("Engine tools (AMUI_WORKSPACE: {})", other.display());
        assert!(named.report().rows.contains(&Row::Section(title)));
        // this repository's build while another workspace is in use: named from the am-ui folder
        let both = Scratch::with(&[("AMUI_WORKSPACE", other.to_str().unwrap())], build_engine);
        both.machine.answer("--check", 0, "", "");
        assert_eq!(both.row(&format!("workspace/{HELPER}")), ("ok", String::new()));
        // a workspace folder that is not there
        let gone = Scratch::with(&[("AMUI_WORKSPACE", "/nowhere/workspace")], |root| {
            fs::remove_dir(root.join("workspace")).unwrap();
        });
        let hint = "unset AMUI_WORKSPACE to use the workspace/ folder of this repository";
        assert_eq!(gone.row("workspace folder"), ("MISSING", hint.to_string()));
        assert_eq!(gone.row(HELPER).0, "MISSING");
        assert_eq!(gone.report().missing_required, 8);
    }

    #[test]
    fn the_renderer_rows() {
        // the patched renderer is not built, an older workspace's script renders meanwhile
        let scratch =
            Scratch::with(&[], |root| write(&root.join("workspace/tools/render-music-916.sh"), "stand-in\n", 0o755));
        let config = &scratch.config;
        assert_eq!(config.render_script, config.workspace_render_script);
        let waiting = "not built: ./setup.sh (or tools/cavern-lab/build.sh); until then the workspace script renders";
        assert_eq!(scratch.row("patched Cavern (tools/cavern-lab)"), ("absent", waiting.to_string()));
        assert_eq!(scratch.row("render script in use"), ("ok", scratch.shown(&config.workspace_render_script)));
        // built, with the bass feed switched on: its generation
        let built = Scratch::with(&[("AMUI_BASS_MANAGE", "on")], build_engine);
        assert_eq!(built.row("patched Cavern (tools/cavern-lab)"), ("ok", "generation cavern-lab-2+bm".to_string()));
        // a script named in the environment that is not there
        let named = Scratch::with(&[("AMUI_RENDER_SCRIPT", "/nowhere/render.sh")], |_| {});
        assert_eq!(named.row("render script in use"), ("MISSING", "/nowhere/render.sh".to_string()));
        assert_eq!(named.row("patched Cavern (tools/cavern-lab)").0, "MISSING");
    }

    #[test]
    fn the_sink_row() {
        // the session's sound server, found through its socket
        let scratch = Scratch::new();
        let server = "sound server";
        let no_session = "no XDG_RUNTIME_DIR: run this in the desktop session";
        assert_eq!(scratch.row(server), ("MISSING", no_session.to_string()));
        let runtime = scratch.dir.path().join("runtime");
        scratch.machine.set_variable("XDG_RUNTIME_DIR", &runtime.display().to_string());
        let socket = runtime.join("pulse/native");
        let gone = format!("{} is not there: is pipewire-pulse (or PulseAudio) running?", socket.display());
        assert_eq!(scratch.row(server), ("MISSING", gone));
        write(&socket, "", 0o600);
        assert_eq!(scratch.row(server), ("ok", socket.display().to_string()));
        // PULSE_SERVER goes first: a socket of its own, or a server elsewhere
        let other = scratch.dir.path().join("other.socket");
        scratch.machine.set_variable("PULSE_SERVER", &format!("unix:{}", other.display()));
        assert_eq!(scratch.row(server).0, "MISSING");
        scratch.machine.set_variable("PULSE_SERVER", "tcp:sound.example:4713");
        assert_eq!(scratch.row(server), ("ok", "tcp:sound.example:4713 (PULSE_SERVER)".to_string()));
        assert_eq!(scratch.report().missing_required, 6, "the sound server counts");

        // the older application's sink: reported when pw-cli can look, never required
        let row = |scratch: &Scratch| {
            let report = scratch.report();
            report.rows.iter().find_map(|row| match row {
                Row::Item { found, item, detail } if item == SINK_NAME => Some((found.as_str(), detail.clone())),
                _ => None,
            })
        };
        assert_eq!(row(&scratch), None, "without pw-cli the sink is not looked for");
        scratch.machine.install(&["pw-cli"]);
        let absent = Some(("absent", "not running (am-ui downmixes on its own)".to_string()));
        assert_eq!(row(&scratch), absent);
        scratch.machine.answer("pw-cli ls Node", 1, "", "Error: \"atmos_916_downmix\" is busy\n"); // any mention counts
        let running = "the older application's downmix sink is running (am-ui does not need it)";
        assert_eq!(row(&scratch), Some(("ok", running.to_string())));
        assert_eq!(scratch.report().missing_required, 6, "the sink does not count");
    }

    #[test]
    fn a_data_folder_that_cannot_be_written() {
        let scratch =
            Scratch::with(&[], |root| fs::write(root.join("var"), "a file where the folder belongs").unwrap());
        let (status, detail) = scratch.row("var/ writable");
        assert_eq!(status, "MISSING");
        // the kind of the system's error, its number and its words
        assert!(
            detail.starts_with("FileExistsError: [Errno 17] ") || detail.starts_with("NotADirectoryError: [Errno 20] "),
            "{detail}"
        );
        assert!(!detail.contains("os error"), "{detail}");
        assert_eq!(os_error_text(&io::Error::from_raw_os_error(13)), "PermissionError: [Errno 13] Permission denied");
        assert_eq!(os_error_text(&io::Error::other("odd")), "OSError: odd");
    }

    #[test]
    fn the_sign_in_rows() {
        let scratch = Scratch::new();
        let config = &scratch.config;
        let item = "Apple Music sign-in";
        assert_eq!(scratch.row(item), ("absent", "none — press Sign In in the app".to_string()));
        // a saved browser session: not verified, then what the last check said
        sign_in(config, Source::Browser, BROWSER_TOKEN);
        let unverified =
            "browser session: saved, not verified yet (./am-ui cli check; the app checks it when it starts)";
        assert_eq!(scratch.row(item), ("absent", unverified.to_string()));
        fs::write(&config.session_status_file, r#"{"state": "reconnect_required", "http_status": 401}"#).unwrap();
        assert_eq!(
            scratch.row(item),
            ("absent", "browser session: rejected by Apple — sign in again with the app's Sign In".to_string())
        );
        let saved = r#"{"state": "connected", "cookie_expires_at": "2027-01-02T03:04:05+00:00", "source": "browser"}"#;
        fs::write(&config.session_status_file, saved).unwrap();
        assert_eq!(
            scratch.row(item),
            ("ok", "browser session: connected, valid until 2027-01-02T03:04:05+00:00".to_string())
        );
        assert_eq!(scratch.row("builds/music-downloader/web-session/"), ("ok", String::new()));
        // the native sign-in comes first; its state is its own
        sign_in(config, Source::Android, ANDROID_TOKEN);
        let unverified =
            "Apple's native sign-in: saved, not verified yet (./am-ui cli check; the app checks it when it starts)";
        assert_eq!(scratch.row(item), ("absent", unverified.to_string()));
        let text = scratch.doctor().0;
        assert!(!text.contains(ANDROID_TOKEN) && !text.contains(BROWSER_TOKEN));
        // nothing here can sign in: the lossless route is not built
        let nothing = "nothing: Apple's native sign-in needs the lossless route, which is not built";
        assert_eq!(scratch.row("what Sign In does here"), ("absent", nothing.to_string()));
        assert_eq!(scratch.report().missing_required, 7); // none of these rows counts
    }

    /// tests/test_signin.py: DoctorStartCheckTest.test_the_doctor_names_a_wrapper_that_cannot_start
    /// (there against the control script itself; here against what it prints).
    #[test]
    fn the_doctor_names_a_wrapper_that_cannot_start() {
        let scratch = Scratch::new();
        let config = &scratch.config;
        build_lossless(config, false);
        scratch.machine.answer("selftest", 0, "ok: the wrapper starts on this system\n", "");
        assert_eq!(lossless_start_problem(config, &scratch.machine), "");
        scratch.machine.answer(
            "selftest",
            1,
            "",
            &format!("unshare: Operation not permitted\nerror: {SELFTEST_FAILURE}\n\n"),
        );
        let problem = lossless_start_problem(config, &scratch.machine);
        let begins = "Apple's component for the sign-in and for lossless cannot start on this computer: this system did \
                      not let it set up the private namespace";
        assert!(problem.starts_with(begins), "{problem}");
        assert!(problem.contains("(unshare: Operation not permitted)"), "{problem}");
        // a failure without words, and a check that cannot be made
        scratch.machine.answer("selftest", 3, "printed elsewhere\n", " \n");
        assert_eq!(lossless_start_problem(config, &scratch.machine), "the start check failed");
        scratch.machine.outcome("selftest", Err(RunError::Timeout));
        assert_eq!(lossless_start_problem(config, &scratch.machine), "");
        let helper = config.lossless_wrapperctl.display();
        assert_eq!(scratch.machine.command_lines().last().unwrap(), &format!("python3 -I {helper} selftest"));
    }

    #[test]
    fn the_lossless_rows() {
        let scratch = Scratch::new();
        let config = &scratch.config;
        let (connect, route) = ("what Sign In does here", "lossless (ALAC) route");
        build_lossless(config, false);
        // the control script cannot be asked (no python3 here): the files decide
        assert_eq!(scratch.row(route), ("absent", "built, not signed in: press Sign In in the app".to_string()));
        assert_eq!(scratch.row(connect), ("ok", "Apple's native sign-in (lossless available)".to_string()));
        assert_eq!(
            scratch.machine.command_lines().last().unwrap(),
            &format!("python3 -I {} build-state", config.lossless_wrapperctl.display())
        );
        // a build that is not staged yet is not started
        scratch.machine.answer("build-state", 0, "unstaged stopped\n", "");
        scratch.machine.answer("selftest", 1, "", &format!("error: {SELFTEST_FAILURE}\n"));
        let update = "; an update is waiting (quit am-ui, tools/lossless/stop.sh, then ./setup.sh)";
        assert_eq!(scratch.row(route), ("absent", format!("built, not signed in: press Sign In in the app{update}")));
        // an older build that cannot start
        scratch.machine.answer("build-state", 0, "stale running\n", "");
        assert_eq!(scratch.row(route), ("absent", format!("built, but {SELFTEST_FAILURE}{update}")));
        let cannot = "Apple's native sign-in, whose component cannot start on this computer (see the lossless line)";
        assert_eq!(scratch.row(connect), ("absent", cannot.to_string()));
        // signed in, current, and still it cannot start
        build_lossless(config, true);
        scratch.machine.answer("build-state", 0, "current stopped\n", "");
        assert_eq!(scratch.row(route), ("absent", format!("built, but {SELFTEST_FAILURE}")));
        scratch.machine.answer("selftest", 0, "", "");
        assert_eq!(scratch.row(route), ("ok", "built and signed in".to_string()));
        // a state the script does not say (it failed) is none
        scratch.machine.answer("build-state", 1, "current running\n", "");
        scratch.machine.answer("selftest", 1, "", "error: not asked\n");
        assert_eq!(scratch.row(route), ("ok", "built and signed in".to_string()));
        scratch.machine.answer("build-state", 0, "\n", "");
        assert_eq!(scratch.row(route), ("ok", "built and signed in".to_string()));
        assert!(!scratch.machine.command_lines().iter().rev().take(2).any(|line| line.ends_with("selftest")));
    }

    #[test]
    fn the_host_finds_programs_along_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        write(&first.join("tool"), "#!/bin/sh\nexit 0\n", 0o644); // not to be run
        fs::create_dir_all(&second).unwrap();
        write_program(&second.join("tool"), "#!/bin/sh\necho out\necho err >&2\nexit 3\n");
        fs::create_dir_all(first.join("folder")).unwrap();
        let path = format!("{}:{}:/nowhere", first.display(), second.display());
        let host =
            Host::new(&HashMap::from([("PATH".to_string(), path), ("AMUI_DOTNET".to_string(), "x".to_string())]));
        assert_eq!(host.which("tool"), Some(second.join("tool")));
        assert_eq!(host.which("folder"), None);
        assert_eq!(host.which("another"), None);
        let named = second.join("tool");
        assert_eq!(host.which(named.to_str().unwrap()), Some(named.clone()));
        assert_eq!(host.which(first.join("tool").to_str().unwrap()), None);
        assert_eq!((host.variable("AMUI_DOTNET").as_deref(), host.variable("HOME")), (Some("x"), None));
        assert_eq!(Host::new(&HashMap::from([("PATH".to_string(), String::new())])).which("tool"), None);
        // the scratch program, run for real: what it printed and how it ended
        let done = host.run(&[named.to_str().unwrap()], Some(Duration::from_secs(20))).unwrap();
        assert_eq!((done.status, done.stdout.as_str(), done.stderr.as_str()), (Some(3), "out\n", "err\n"));
        assert_eq!(printed(&host, &[named.to_str().unwrap()], 20).as_deref(), Some("out\nerr\n"));
        assert_eq!(printed(&host, &[first.join("missing").to_str().unwrap()], 20), None);
    }
}
