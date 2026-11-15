//! The command line from the outside: `amui::cli` through its public parts only, and the `amui`
//! program itself.
//!
//! The first half builds a [`Context`] by hand, the way a program with commands of its own
//! would, from stand-ins for everything a command reaches: the network, the wrapper service, the
//! sink, other programs, the sign-in, the clock, Ctrl+C. The second half starts the program
//! with an environment that names a scratch am-ui folder and nothing else, so it finds no real
//! configuration, data folder, sign-in or sink, and only runs commands that work offline.
//!
//! Everything is synthetic; the commands themselves are tested next to their code
//! (`src/cli/`), against what the Python command line printed.

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command as Program;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use amui::acquire::{AcquireError, Completed};
use amui::cli::{self, Command, Console, Context, Interrupt};
use amui::config::Config;
use amui::doctor::Machine;
use amui::downmix::{Downmix, Output, Paths, RunError};
use amui::lossless::{Lossless, Service, Wrapperctl};
use amui::session::SessionError;
use amui::util::Event;
use amui::web::{Call, Exchange, Head, Http, NetError, Progress, Source, Transport, Web, WebError};
use serde_json::json;

// --- stand-ins ------------------------------------------------------------------

/// The network of the API client: one library page, and a note of what was asked.
struct Library {
    asked: Mutex<Vec<String>>,
}

impl Http for Library {
    fn request(&self, call: &Call) -> Result<Vec<u8>, WebError> {
        self.asked.lock().unwrap().push(call.url.to_string());
        if !call.url.ends_with("/v1/me/library/albums?limit=2") {
            return Err(WebError::NotFound("amp-api.music.apple.com returned HTTP 404".to_string()));
        }
        let album = |id: &str, name: &str| json!({"id": id, "type": "library-albums", "attributes": {"name": name, "artistName": "Artist A", "trackCount": 3}});
        let page = json!({"data": [album("l.AbCdEf1", "Album A"), album("l.AbCdEf2", "Album B")],
                          "next": "/v1/me/library/albums?offset=2", "meta": {"total": 5}});
        Ok(page.to_string().into_bytes())
    }

    fn download(
        &self,
        _: &str,
        _: &Path,
        _: u64,
        _: Option<Progress>,
        _: Option<&Event>,
        _: f64,
    ) -> Result<(u64, String), WebError> {
        panic!("these commands download nothing");
    }

    fn token_sources(&self) -> Vec<Source> {
        Vec::new()
    }
}

/// A network the sign-in's own requests may not reach.
struct NoNetwork;

impl Transport for NoNetwork {
    fn open(&self, _: &Exchange) -> Result<(Head, Box<dyn Read>), NetError> {
        panic!("these commands ask nothing of the network themselves");
    }
}

/// A wrapper service that does not answer, and a control script that is never run.
struct NoService;

impl Service for NoService {
    fn get(&self, _: &str, _: f64) -> Result<Vec<u8>, String> {
        Err("ConnectionRefusedError".to_string())
    }
}

impl Wrapperctl for NoService {
    fn run(&self, _: &str, _: f64) -> Result<Completed, amui::acquire::RunError> {
        panic!("these commands start no service");
    }

    fn detach(&self, _: &str) -> io::Result<()> {
        Ok(())
    }
}

/// A machine without programs.
struct Bare;

impl Machine for Bare {
    fn which(&self, _: &str) -> Option<PathBuf> {
        None
    }

    fn run(&self, _: &[&str], _: Option<Duration>) -> Result<Output, RunError> {
        Err(RunError::NotFound)
    }

    fn variable(&self, _: &str) -> Option<String> {
        None
    }
}

/// Nobody presses Ctrl+C.
struct Quiet;

impl Interrupt for Quiet {
    fn asked(&self) -> bool {
        false
    }
}

/// A scratch am-ui folder and a context over it, made of the stand-ins above.
struct Scratch {
    _dir: tempfile::TempDir,
    context: Context,
    library: Arc<Library>,
}

impl Scratch {
    fn new() -> Scratch {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("am-ui");
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        let config = Arc::new(Config::new(root, &dir.path().join("home"), &HashMap::new()));
        let library = Arc::new(Library { asked: Mutex::default() });
        let http: Arc<dyn Http> = library.clone();
        let no_sink = |_: &[&str], _: Option<Duration>| -> Result<Output, RunError> { Err(RunError::NotFound) };
        let context = Context {
            web: Arc::new(Web::with_transport(config.clone(), Box::new(NoNetwork))),
            lossless: Arc::new(Lossless::with_parts(
                config.clone(),
                http.clone(),
                Box::new(NoService),
                Box::new(NoService),
            )),
            downmix: Arc::new(Downmix::with(config.clone(), Paths::new(&config), Box::new(no_sink))),
            machine: Arc::new(Bare),
            signin: Box::new(|_| Err(SessionError::NoTerminal)),
            clock: Box::new(|| 0.0),
            decode: Box::new(|_, _| Err(AcquireError::other("nothing is decoded here"))),
            interrupt: Arc::new(Quiet),
            config,
            http,
        };
        Scratch { _dir: dir, context, library }
    }

    /// Run a command line of `table`: (exit status, standard output, standard error).
    fn run(&self, table: &[Command], args: &[&str]) -> (i32, String, String) {
        let args: Vec<String> = args.iter().map(|word| word.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let status = cli::dispatch(&self.context, table, &args, &mut Console::new(&mut out, &mut err));
        (status, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }
}

// --- the library's public parts -------------------------------------------------------

#[test]
fn commands_run_on_a_context_made_by_hand() {
    let scratch = Scratch::new();
    let table = cli::commands();
    // offline: the state of a machine without a sign-in
    let (status, out, err) = scratch.run(&table, &["status"]);
    assert_eq!((status, out.as_str(), err.as_str()), (1, "state: no_session\ntoken file present: False\n", ""));
    // through the network handed in
    let (status, out, err) = scratch.run(&table, &["albums", "--limit", "2"]);
    assert_eq!(
        out,
        "key        name     artist    count\n\
         ---------  -------  --------  -----\n\
         l.AbCdEf1  Album A  Artist A  3    \n\
         l.AbCdEf2  Album B  Artist A  3    \n\
         2 albums of 5; next --offset 2\n"
    );
    assert_eq!((status, err.as_str()), (0, ""));
    assert_eq!(
        *scratch.library.asked.lock().unwrap(),
        ["https://amp-api.music.apple.com/v1/me/library/albums?limit=2"]
    );
    // what the network does not have
    let (status, out, err) = scratch.run(&table, &["albums"]);
    assert_eq!((status, out.as_str()), (1, ""));
    assert_eq!(err, "Error: amp-api.music.apple.com returned HTTP 404\n");
    // through the sink, the service and the machine handed in
    let (status, out, _) = scratch.run(&table, &["downmix"]);
    assert_eq!(status, 0);
    assert_eq!(out, "current: preset=peqdb preamp=-11.0 dB match=True (from workspace); sink running: False\n");
    let (status, out, _) = scratch.run(&table, &["lossless"]);
    assert_eq!(status, 0);
    assert!(out.starts_with("  built            False\n") && out.ends_with("x86_64)\n"), "{out}");
    let (status, out, err) = scratch.run(&table, &["doctor"]);
    assert_eq!((status, err.as_str()), (1, ""));
    assert!(out.starts_with("\nPrograms\n  MISSING  ffmpeg "), "{out}");
    assert!(out.contains("\n  absent   Apple Music sign-in                none — press Sign In in the app\n"), "{out}");
    // the sign-in handed in
    let (status, _, err) = scratch.run(&table, &["connect"]);
    assert_eq!(status, 1);
    assert!(err.starts_with("Error: The native sign-in asks for the password"), "{err}");
}

#[test]
fn a_program_adds_its_own_commands_to_the_table() {
    fn cmd_play(context: &Context, args: &[String], console: &mut Console) -> i32 {
        if args.is_empty() {
            console.error("usage: amui play <id>...");
            return 2;
        }
        console.line(format!("would play {} from {}", args.join(", "), context.config.media_cache_dir.display()));
        0
    }
    let scratch = Scratch::new();
    let mut table = cli::commands();
    let before = table.len();
    table.push(Command { name: "play", help: "play catalog ids through the engine", run: cmd_play });
    let media = scratch.context.config.media_cache_dir.display().to_string();
    assert_eq!(
        scratch.run(&table, &["play", "1000000001", "1000000002"]),
        (0, format!("would play 1000000001, 1000000002 from {media}\n"), String::new())
    );
    assert_eq!(scratch.run(&table, &["play"]), (2, String::new(), "usage: amui play <id>...\n".to_string()));
    let (status, help, _) = scratch.run(&table, &["help"]);
    assert_eq!(status, 0);
    assert!(help.contains("\n  play         play catalog ids through the engine\n"), "{help}");
    assert_eq!(help.lines().count(), 2 + before + 1);
    // without it the name is unknown, with the hint the program gives
    let (status, out, err) = scratch.run(&cli::commands(), &["play", "1000000001"]);
    assert_eq!((status, out.as_str(), err.as_str()), (2, "", "unknown command: play (try: amui help)\n"));
}

// --- the program ------------------------------------------------------------------------

/// The `amui` program over a scratch am-ui folder: its environment names that folder and
/// nothing of the machine's own.
struct Installed {
    dir: tempfile::TempDir,
}

impl Installed {
    fn new() -> Installed {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("am-ui/workspace")).unwrap();
        Installed { dir }
    }

    fn var(&self) -> PathBuf {
        self.dir.path().join("am-ui/var")
    }

    /// (exit status, standard output, standard error) of `amui <args>`.
    fn run(&self, args: &[&str]) -> (Option<i32>, String, String) {
        let done = Program::new(env!("CARGO_BIN_EXE_amui"))
            .args(args)
            .env_clear()
            .env("AMUI_ROOT", self.dir.path().join("am-ui"))
            .env("HOME", self.dir.path().join("home"))
            .env("PATH", self.dir.path().join("no-programs"))
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let text = |bytes: Vec<u8>| String::from_utf8(bytes).unwrap();
        (done.status.code(), text(done.stdout), text(done.stderr))
    }
}

#[test]
fn the_program_lists_its_commands() {
    let installed = Installed::new();
    let (status, out, err) = installed.run(&["help"]);
    assert_eq!((status, err.as_str()), (Some(0), ""));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[..2], ["usage: amui <command> [args]", ""]);
    let names: Vec<&str> = lines[2..].iter().map(|line| line.split_whitespace().next().unwrap()).collect();
    let mut expected: Vec<&str> = cli::commands().iter().map(|command| command.name).collect();
    expected.sort_unstable();
    assert_eq!(names, expected);
    assert_eq!(installed.run(&[]), (Some(0), out, String::new()));
    assert!(!installed.var().exists()); // the list touches nothing
}

#[test]
fn the_program_ends_with_the_status_of_its_command() {
    let installed = Installed::new();
    // a name it does not know, a command line it cannot read: status 2, on standard error
    assert_eq!(
        installed.run(&["sing", "1000000001"]),
        (Some(2), String::new(), "unknown command: sing (try: amui help)\n".to_string())
    );
    let (status, out, err) = installed.run(&["tracks"]);
    assert_eq!((status, out.as_str()), (Some(2), ""));
    assert_eq!(
        err,
        "usage: amui tracks [-h] [--offset OFFSET] [--limit LIMIT] key\n\
         amui tracks: error: the following arguments are required: key\n"
    );
    let (status, out, err) = installed.run(&["tracks", "--help"]);
    assert_eq!((status, err.as_str()), (Some(0), ""));
    assert!(out.starts_with("usage: amui tracks [-h] [--offset OFFSET] [--limit LIMIT] key\n\nTracks of "), "{out}");
    assert!(!installed.var().exists());
    // a command that works offline: no sign-in on this scratch folder is status 1
    assert_eq!(
        installed.run(&["status"]),
        (Some(1), "state: no_session\ntoken file present: False\n".to_string(), String::new())
    );
    // a command that succeeds, in the data folder of the scratch folder
    let (status, out, err) = installed.run(&["cache"]);
    assert_eq!((status, err.as_str()), (Some(0), ""));
    let var = installed.var().display().to_string();
    assert_eq!(
        out,
        format!(
            "media cache {var}/cache/media (0 B of 2.0 GiB):\n\
             pcm cache {var}/cache/pcm (keep 3 songs):\n\
             art cache {var}/cache/art: 0 files, 0 B of 256.0 MiB\n"
        )
    );
    assert!(installed.var().join("cache/media").is_dir());
}
