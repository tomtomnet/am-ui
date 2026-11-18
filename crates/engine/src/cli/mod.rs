//! `amui <command> [args]`: script-level access to the engine, without a user interface.
//!
//! * `doctor` ([`crate::doctor`]) says what this machine has and lacks.
//! * [`library`]: the sign-in, the library, search, metadata and lyrics.
//! * [`pipeline`]: acquisition, render, decode, downmix and the caches.
//!
//! `amui help` lists the commands, `amui <command> -h` prints the help of one. The Python
//! command line (`amui/__main__.py`, `cli_library.py`, `cli_pipeline.py`) had the same commands
//! with the same options, output and exit statuses. Its `play` and `output-test` (they need the
//! player) and the lossless route's control script (`wrapper`) are ported separately and are not
//! in the table yet.
//!
//! # The table
//!
//! A [`Command`] is a name, one line for `amui help` and a function that gets the [`Context`],
//! the words after the command's name and the [`Console`], and returns the exit status. Each
//! module has its `COMMANDS`; [`commands`] puts them together and [`dispatch`] runs one of any
//! table. To add a command: write its function next to its [`Spec`] (what it takes on its
//! command line), put a [`Command`] for it into the module's `COMMANDS`, and a new module's
//! `COMMANDS` into [`commands`]. A program with commands of its own hands [`dispatch`] the table
//! it wants:
//!
//! ```text
//! let mut table = cli::commands();
//! table.push(Command { name: "play", help: "play catalog ids / Apple Music URLs", run: cmd_play });
//! let status = cli::dispatch(&context, &table, &args, &mut console);
//! ```
//!
//! A later entry replaces an earlier one of the same name.
//!
//! # Output and the outside world
//!
//! Commands write through the [`Console`] they are handed, never to the process's streams
//! directly, and reach everything outside the process through the [`Context`]: requests through
//! its `http`, the sign-in through its `web`, the wrapper service through its `lossless`, the
//! sink through its `downmix`, other programs through its `machine`, Ctrl+C through its
//! `interrupt`. [`Context::new`] wires the real ones for [`run`]; tests hand in stand-ins and
//! read what was printed.
//!
//! No command prints a token, a signed URL or a key: `status` prints the state only, the
//! acquisition commands progress and what ffprobe says of the file.
//!
//! # Differences from the Python command line
//!
//! * The program is `amui`: the usage lines, the hint for an unknown command and "run: amui
//!   acquire ..." say so where Python said `python3 -m amui`. Hints that name the launcher
//!   (`./am-ui cli connect`, `./am-ui cli check`) are unchanged.
//! * Nothing is loaded at run time, so there are no "(module unavailable: ...)" lines.
//! * Arguments are parsed by hand ([`args`] lists what differs from `argparse`).
//! * `connect` is Apple's native sign-in only. The engine has no browser sign-in on purpose:
//!   `connect --browser` says so and ends with status 2, `--timeout` (the browser sign-in's
//!   time limit) is accepted and not used, and the texts that offered the browser are gone.
//! * `doctor` takes `-h`, and other arguments are a usage error; Python's ignored its arguments.
//!   [`crate::doctor`] lists which rows of the report no longer apply.
//! * Ctrl+C. `render` stops its renderer, prints "stopped" and ends with status 130, as before.
//!   `acquire` cancels the acquisition, which removes its unfinished files, prints "cancelled"
//!   and ends with status 130 (Python ended with a traceback and the same status). Both take
//!   the first Ctrl+C over ([`Interrupt`]); a second one ends the process at once. The other
//!   commands are ended by the signal itself (status 130 in a shell), without the line
//!   "Interrupted" of Python's library commands.
//! * What Python left as a traceback (exit status 1) is one line `error: ...` with the same
//!   status: a cache entry another process is using, a cache index or data folder that cannot
//!   be written, a control script that cannot be started for `lossless --stop`.
//! * `lossless --stop` shows what the control script printed once it has ended (Python let it
//!   write to the terminal itself).
//! * Values that Python printed in its own spelling keep it ([`text`]). A library key is numeric
//!   when it is ASCII digits, as everywhere in this engine (Python's `isdigit()` also took the
//!   digits of other scripts).

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once, OnceLock};
use std::time::Instant;

use crate::acquire::{self, AcquireError};
use crate::config::Config;
use crate::doctor::{self, Host, Machine};
use crate::downmix::Downmix;
use crate::lossless::Lossless;
use crate::session::{self, SessionError};
use crate::web::{Http, Web};

pub mod args;
pub mod library;
pub mod pipeline;
pub mod text;

use args::{Spec, Stop};

/// The name of the program in usage lines and hints.
pub const PROGRAM: &str = "amui";

/// The native sign-in in the terminal of this process: true when it produced a fresh sign-in.
pub type TerminalSignIn = Box<dyn Fn(&Web) -> Result<bool, SessionError> + Send + Sync>;
/// Seconds on a clock that never goes back.
pub type Clock = Box<dyn Fn() -> f64 + Send + Sync>;
/// Decode a cached stereo file (AAC or ALAC) to an f32le file; returns the frames written.
pub type Decode = Box<dyn Fn(&Path, &Path) -> Result<u64, AcquireError> + Send + Sync>;

/// What the commands work with: the configuration and every part of the engine through which a
/// command reaches something outside the process.
pub struct Context {
    pub config: Arc<Config>,
    /// The HTTP layer itself: the sign-in's status, its live check and the native sign-in.
    pub web: Arc<Web>,
    /// What the API client and the acquisition send their requests through (the same `Web`).
    pub http: Arc<dyn Http>,
    /// The lossless route: its service on loopback and its control script.
    pub lossless: Arc<Lossless>,
    /// The downmix sink.
    pub downmix: Arc<Downmix>,
    /// Other programs of this machine: what the doctor looks for and runs, and the lossless
    /// route's control script for `lossless --stop`.
    pub machine: Arc<dyn Machine>,
    /// `connect`: [`session::android_signin_terminal`].
    pub signin: TerminalSignIn,
    /// The elapsed times the commands print.
    pub clock: Clock,
    /// `decode`: [`acquire::decode_stereo`] (ffprobe and ffmpeg).
    pub decode: Decode,
    /// Ctrl+C, for the commands that tidy up before they end.
    pub interrupt: Arc<dyn Interrupt>,
}

/// Seconds since the first look at this clock.
fn monotonic() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// Ctrl+C as a command sees it that has something to tidy up before it ends (`acquire` its
/// unfinished files, `render` its renderer).
pub trait Interrupt: Send + Sync {
    /// Whether the person at the terminal asked to stop.
    ///
    /// A command that asks takes Ctrl+C over from then on: the first one no longer ends the
    /// process but is reported here, for the command to end in its own way; a second one ends
    /// the process at once. A command that never asks is ended by the signal as usual.
    fn asked(&self) -> bool;
}

/// The interrupt signal of this process (SIGINT).
#[derive(Debug, Clone, Copy, Default)]
pub struct CtrlC;

/// Set by the signal handler: the one thing it may safely do. A process has one terminal and
/// one such signal, so this is a static like the handler itself.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_interrupt(_signal: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
    // the next one ends the process, as it would have without this handler
    // SAFETY: signal() may be called from a signal handler (it is async-signal-safe), and
    // SIG_DFL is a valid disposition.
    unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
}

impl Interrupt for CtrlC {
    fn asked(&self) -> bool {
        static TAKEN_OVER: Once = Once::new();
        TAKEN_OVER.call_once(|| {
            let handler = on_interrupt as extern "C" fn(libc::c_int);
            // SAFETY: the handler only stores into an atomic and calls signal(), both of which a
            // signal handler may do; programs started later get the default again when they are
            // executed.
            let before = unsafe { libc::signal(libc::SIGINT, handler as libc::sighandler_t) };
            if before == libc::SIG_IGN {
                // started with the signal ignored (a background job of a script): it stays so
                // SAFETY: SIG_IGN is a valid disposition.
                unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
            }
        });
        INTERRUPTED.load(Ordering::SeqCst)
    }
}

impl Context {
    /// The engine of this process over `config`: the real network, service, sink and programs.
    /// Nothing is contacted or started before a command asks for it.
    pub fn new(config: Arc<Config>) -> Context {
        let web = Arc::new(Web::new(config.clone()));
        let http: Arc<dyn Http> = web.clone();
        Context {
            lossless: Arc::new(Lossless::new(config.clone(), http.clone())),
            downmix: Arc::new(Downmix::new(config.clone())),
            machine: Arc::new(Host::from_process()),
            signin: Box::new(session::android_signin_terminal),
            clock: Box::new(monotonic),
            decode: Box::new(acquire::decode_stereo),
            interrupt: Arc::new(CtrlC),
            config,
            web,
            http,
        }
    }
}

/// Where a command writes: its results, and what is for the eyes of the person at the terminal
/// only. A reader that went away is not an error of the command.
pub struct Console<'a> {
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
}

impl<'a> Console<'a> {
    pub fn new(out: &'a mut dyn Write, err: &'a mut dyn Write) -> Console<'a> {
        Console { out, err }
    }

    /// One line on standard output.
    pub fn line(&mut self, text: impl AsRef<str>) {
        let _ = writeln!(self.out, "{}", text.as_ref());
    }

    /// One line on standard error.
    pub fn error(&mut self, text: impl AsRef<str>) {
        let _ = writeln!(self.err, "{}", text.as_ref());
    }

    /// Text as it is on standard output (what another program printed).
    pub fn pass(&mut self, text: &str) {
        let _ = self.out.write_all(text.as_bytes());
    }

    /// Text as it is on standard error.
    pub fn pass_error(&mut self, text: &str) {
        let _ = self.err.write_all(text.as_bytes());
    }

    /// Standard output itself.
    pub fn out(&mut self) -> &mut dyn Write {
        &mut *self.out
    }

    /// Print why a command line ended before its command ran (the help, or the usage error);
    /// returns the exit status that goes with it.
    pub fn stop(&mut self, stop: &Stop) -> i32 {
        match stop {
            Stop::Help(text) => self.line(text),
            Stop::Usage(text) => self.error(text),
        }
        stop.status()
    }
}

/// What a command's function gets: the context, the words after the command's name, the console.
/// It returns the exit status.
pub type Run = fn(&Context, &[String], &mut Console) -> i32;

/// One entry of the command table.
#[derive(Clone, Copy)]
pub struct Command {
    pub name: &'static str,
    /// One line for `amui help`.
    pub help: &'static str,
    pub run: Run,
}

const DOCTOR: Spec = Spec {
    name: "doctor",
    description: "What this machine has and lacks to run am-ui: programs, engine tools, renderer, sink, sign-in",
    options: &[],
    arguments: &[],
    exclusive: &[],
};

fn cmd_doctor(context: &Context, args: &[String], console: &mut Console) -> i32 {
    if let Err(stop) = DOCTOR.parse(args) {
        return console.stop(&stop);
    }
    doctor::run(&context.web, context.machine.as_ref(), console.out())
}

/// The doctor's entry of the table (Python: `COMMANDS` of `amui/doctor.py`).
pub const COMMANDS: &[Command] = &[Command {
    name: "doctor",
    help: "what this machine has and lacks: engine tools, programs, sink, session",
    run: cmd_doctor,
}];

/// Every command this module tree has: the doctor's, the library's, the pipeline's.
pub fn commands() -> Vec<Command> {
    [COMMANDS, library::COMMANDS, pipeline::COMMANDS].concat()
}

/// Run the command the first word names with the words after it; without a word, or for `help`,
/// `-h` and `--help`, list the commands. Returns the exit status: the command's own, 0 for the
/// list, 2 for a name that is not in the table.
pub fn dispatch(context: &Context, commands: &[Command], args: &[String], console: &mut Console) -> i32 {
    let named = args.split_first().filter(|(name, _)| !matches!(name.as_str(), "-h" | "--help" | "help"));
    let Some((name, rest)) = named else {
        console.line(format!("usage: {PROGRAM} <command> [args]\n"));
        let by_name: BTreeMap<&str, &Command> = commands.iter().map(|command| (command.name, command)).collect();
        for command in by_name.values() {
            console.line(format!("  {:<12} {}", command.name, command.help));
        }
        return 0;
    };
    match commands.iter().rev().find(|command| command.name == name) {
        Some(command) => (command.run)(context, rest, console),
        None => {
            console.error(format!("unknown command: {name} (try: {PROGRAM} help)"));
            2
        }
    }
}

/// The command line of this process: `args` are the words after the program's name, the output
/// goes to the process's standard streams. Returns the exit status.
pub fn run(config: Arc<Config>, args: &[String]) -> i32 {
    let context = Context::new(config);
    let (mut out, mut err) = (io::stdout(), io::stderr());
    dispatch(&context, &commands(), args, &mut Console::new(&mut out, &mut err))
}

#[cfg(test)]
pub(crate) mod testing {
    //! A scratch engine for the tests of the commands: a scratch am-ui folder and stand-ins for
    //! the network, the wrapper service, the sink, other programs, the sign-in, the clock and
    //! Ctrl+C.
    use super::*;
    use crate::acquire::testing::{Asked, FakeHttp};
    use crate::acquire::{Completed, RunError};
    use crate::doctor::testing::FakeMachine;
    use crate::downmix::Paths;
    use crate::downmix::testing::FakeSink;
    use crate::lossless::{Service, Wrapperctl};
    use crate::util;
    use crate::web::WebError;
    use crate::web::testing::{Answer, FakeTransport, Seen};
    use serde_json::{Map, Value, json};
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    /// What a command left behind.
    #[derive(Debug, PartialEq, Eq)]
    pub struct Ran {
        pub status: i32,
        pub out: String,
        pub err: String,
    }

    /// The wrapper service and its control script, as far as the commands ask them.
    #[derive(Clone)]
    pub struct Route {
        /// Storefronts the service reports; `None`: nothing answers.
        pub regions: Arc<Mutex<Option<Vec<String>>>>,
        /// The "data" of the service's `/m3u8` answer.
        pub dispatch: Arc<Mutex<Value>>,
        /// The paths asked of the service.
        pub asked: Arc<Mutex<Vec<String>>>,
    }

    impl Service for Route {
        fn get(&self, path: &str, _timeout: f64) -> Result<Vec<u8>, String> {
            self.asked.lock().unwrap().push(path.to_string());
            let data = if path == "/status" {
                match self.regions.lock().unwrap().as_ref() {
                    Some(regions) => json!({ "regions": regions }),
                    None => return Err("ConnectionRefusedError".to_string()),
                }
            } else {
                self.dispatch.lock().unwrap().clone()
            };
            Ok(json!({"code": 0, "msg": "SUCCESS", "data": data}).to_string().into_bytes())
        }
    }

    impl Wrapperctl for Route {
        fn run(&self, _command: &str, _timeout: f64) -> Result<Completed, RunError> {
            Err(RunError::TimedOut) // the commands under test start no service
        }

        fn detach(&self, _command: &str) -> io::Result<()> {
            Ok(())
        }
    }

    /// Ctrl+C of a test: pressed once it has been asked about `quiet` times (never, at first).
    pub struct Presses {
        pub quiet: AtomicUsize,
        asked: AtomicUsize,
    }

    impl Interrupt for Presses {
        fn asked(&self) -> bool {
            self.asked.fetch_add(1, Ordering::SeqCst) >= self.quiet.load(Ordering::SeqCst)
        }
    }

    pub struct Scratch {
        pub dir: tempfile::TempDir,
        pub config: Arc<Config>,
        pub context: Context,
        /// What the API client and the acquisition asked.
        pub http: Arc<FakeHttp>,
        /// What reached the network through the `Web` itself (the live check of the sign-in).
        pub wire: Arc<Mutex<Vec<Seen>>>,
        /// The HTTP status the network answers the `Web` with; 200 comes with a library page.
        pub verdict: Arc<Mutex<u16>>,
        pub route: Route,
        pub sink: Arc<FakeSink>,
        pub machine: Arc<FakeMachine>,
        /// What the clock advances by at every look (0: time stands still).
        pub tick: Arc<Mutex<f64>>,
        pub interrupt: Arc<Presses>,
    }

    impl Scratch {
        /// A scratch engine whose network nothing may use.
        pub fn new() -> Scratch {
            Scratch::with(&[], |_| Err(WebError::other("network used")))
        }

        /// A scratch engine over an environment, with `answer` as the network of the API client
        /// and the acquisition. `{scratch}` in a value of the environment stands for the scratch
        /// folder (the am-ui folder is `{scratch}/am-ui`).
        pub fn with(
            env: &[(&str, &str)],
            answer: impl Fn(&Asked) -> Result<Vec<u8>, WebError> + Send + Sync + 'static,
        ) -> Scratch {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("am-ui");
            fs::create_dir_all(root.join("workspace")).unwrap();
            let scratch = dir.path().display().to_string();
            let mut env: HashMap<String, String> =
                env.iter().map(|(name, value)| (name.to_string(), value.replace("{scratch}", &scratch))).collect();
            env.entry("LOSSLESS_PORT".to_string()).or_insert_with(|| "9".to_string()); // nothing listens there
            let config = Arc::new(Config::new(root, &dir.path().join("home"), &env));
            config.ensure_dirs().unwrap();

            let verdict = Arc::new(Mutex::new(500));
            let status = verdict.clone();
            let (transport, wire) = FakeTransport::new(move |_| {
                Ok(match *status.lock().unwrap() {
                    200 => Answer::ok(br#"{"data": [{"id": "x"}], "next": "/more"}"#),
                    refused => Answer::status(refused),
                })
            });
            let web = Arc::new(Web::with_transport(config.clone(), Box::new(transport)));
            web.preset_developer_token("developer", util::unix_now() + 90.0 * 86_400.0);

            let http = Arc::new(FakeHttp::new(answer));
            let route = Route {
                regions: Arc::new(Mutex::new(None)),
                dispatch: Arc::new(Mutex::new(json!({}))),
                asked: Arc::default(),
            };
            let lossless =
                Lossless::with_parts(config.clone(), http.clone(), Box::new(route.clone()), Box::new(route.clone()));
            let sink = FakeSink::new(Some(38));
            let paths = Paths {
                proc_cgroup: dir.path().join("proc-self-cgroup"),
                cgroup_root: dir.path().join("sys-fs-cgroup"),
                ..Paths::new(&config)
            };
            let machine = Arc::new(FakeMachine::new());
            let interrupt = Arc::new(Presses { quiet: AtomicUsize::new(usize::MAX), asked: AtomicUsize::new(0) });
            let tick = Arc::new(Mutex::new(0.0));
            let (step, now) = (tick.clone(), Mutex::new(100.0));
            let context = Context {
                config: config.clone(),
                web,
                http: http.clone(),
                lossless: Arc::new(lossless),
                downmix: Arc::new(Downmix::with(config.clone(), paths, sink.runner())),
                machine: machine.clone(),
                signin: Box::new(|_| panic!("this test signs nobody in")),
                clock: Box::new(move || {
                    let mut now = now.lock().unwrap();
                    *now += *step.lock().unwrap();
                    *now
                }),
                decode: Box::new(|_, _| panic!("this test decodes nothing")),
                interrupt: interrupt.clone(),
            };
            Scratch { dir, config, context, http, wire, verdict, route, sink, machine, tick, interrupt }
        }

        /// Run a command line (the words after the program's name) of the whole table.
        pub fn run(&self, args: &[&str]) -> Ran {
            let args: Vec<String> = args.iter().map(|word| word.to_string()).collect();
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let status = dispatch(&self.context, &commands(), &args, &mut Console::new(&mut out, &mut err));
            Ran { status, out: String::from_utf8(out).unwrap(), err: String::from_utf8(err).unwrap() }
        }

        /// The URLs the API client and the acquisition asked for, in their order.
        pub fn asked(&self) -> Vec<String> {
            self.http.asked.lock().unwrap().iter().map(|asked| asked.url.clone()).collect()
        }
    }

    /// What the Python command line printed (tests/fixtures/cli_reference.json, made by
    /// tests/reference/cli_reference.py): synthetic answers of the network, states of a scratch
    /// folder with the commands run in them, and what the Python helpers return.
    pub fn reference() -> Value {
        serde_json::from_str(include_str!("../../tests/fixtures/cli_reference.json")).expect("the reference file")
    }

    /// A network that serves answers by URL, as the reference's stand-in network does: a JSON
    /// answer, the text of a playlist (`{"__text__": ...}`) or a rejected session
    /// (`{"__refused__": 401}`). A URL it does not know is a 404.
    pub fn network(
        answers: Map<String, Value>,
    ) -> impl Fn(&Asked) -> Result<Vec<u8>, WebError> + Send + Sync + 'static {
        move |asked| {
            let host = url::Url::parse(&asked.url).ok().and_then(|url| url.host_str().map(str::to_owned));
            let host = host.unwrap_or_default();
            let answer = answers.get(&asked.url).filter(|_| asked.method == "GET");
            let Some(answer) = answer else { return Err(WebError::NotFound(format!("{host} returned HTTP 404"))) };
            if let Some(status) = answer.get("__refused__").and_then(Value::as_u64) {
                let said = format!("{host} rejected the session (HTTP {status}); reconnect to Apple Music");
                return Err(WebError::auth(said, u16::try_from(status).ok()));
            }
            match answer.get("__text__").and_then(Value::as_str) {
                Some(playlist) => Ok(playlist.as_bytes().to_vec()),
                None => Ok(answer.to_string().into_bytes()),
            }
        }
    }

    /// The network of the reference.
    pub fn reference_network() -> impl Fn(&Asked) -> Result<Vec<u8>, WebError> + Send + Sync + 'static {
        network(reference()["answers"].as_object().expect("the answers of the reference").clone())
    }
}

#[cfg(test)]
mod reference;

#[cfg(test)]
mod tests {
    use super::testing::Scratch;
    use super::*;

    fn listed(context: &Context, table: &[Command], args: &[&str]) -> (i32, String, String) {
        let args: Vec<String> = args.iter().map(|word| word.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let status = dispatch(context, table, &args, &mut Console::new(&mut out, &mut err));
        (status, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    #[test]
    fn help_lists_every_command_by_name() {
        let scratch = Scratch::new();
        let expected = "\
usage: amui <command> [args]

  acquire      acquire <id|url> [--stereo|--lossless]: license, download and decrypt into the media cache
  albums       library albums [--offset N] [--limit N]
  artists      library artists [--offset N] [--limit N]
  atmos-check  atmos-check <id|url>: manifest-only Dolby Atmos availability
  cache        cache [--evict]: list the media/PCM/art caches
  charts       storefront charts (top 20 songs/albums/playlists)
  check        verify the saved session live (read-only)
  connect      sign in to Apple Music: the native sign-in in this terminal
  decode       decode <id> [--force] [--lossless]: decode the cached stereo AAC/ALAC file to f32le
  doctor       what this machine has and lacks: engine tools, programs, sink, session
  downmix      downmix [key] [--preamp dB] [--list] [--live]: show or switch the live downmix
  home         recently played + recommendation titles
  lossless     lossless [--start] [--stop] [--check SONG]: state of the ALAC route (wrapper, sign-in, service)
  lyrics       lyrics timing summary: lyrics <catalog id> [--show]
  playlists    library playlists [--offset N] [--limit N]
  recent       recently played albums and playlists
  recently-added recently added library albums/playlists [--offset N] [--limit N]
  render       render <id> [--force]: progressive Cavern 9.1.6 render of the cached Atmos file
  search       search the library and the catalog: search <term> [--limit N]
  song         catalog song details incl. audioTraits: song <catalog id>
  songs        library songs [--offset N] [--limit N]
  status       saved Apple Music session state (offline)
  tracks       tracks of l.<album>, p.<playlist>, pl.<catalog playlist> or a catalog album id
";
        for args in [&[][..], &["help"], &["-h"], &["--help"], &["help", "albums"]] {
            let ran = scratch.run(args);
            assert_eq!((ran.status, ran.out.as_str(), ran.err.as_str()), (0, expected, ""), "{args:?}");
        }
        assert!(scratch.asked().is_empty() && scratch.machine.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn unknown_command() {
        let scratch = Scratch::new();
        let ran = scratch.run(&["play", "1000000001"]);
        assert_eq!((ran.status, ran.out.as_str()), (2, ""));
        assert_eq!(ran.err, "unknown command: play (try: amui help)\n");
        let ran = scratch.run(&["Help"]);
        assert_eq!((ran.status, ran.err.as_str()), (2, "unknown command: Help (try: amui help)\n"));
    }

    #[test]
    fn a_table_takes_more_entries() {
        fn cmd_play(context: &Context, args: &[String], console: &mut Console) -> i32 {
            console.line(format!("play {} with {}", args.join(" "), context.config.root.display()));
            7
        }
        fn cmd_status(_: &Context, _: &[String], console: &mut Console) -> i32 {
            console.error("another status");
            3
        }
        let scratch = Scratch::new();
        let mut table = commands();
        table.push(Command { name: "play", help: "play catalog ids", run: cmd_play });
        table.push(Command { name: "status", help: "replaced", run: cmd_status });
        let root = scratch.config.root.display().to_string();
        assert_eq!(
            listed(&scratch.context, &table, &["play", "1000000001", "--mode", "stereo"]),
            (7, format!("play 1000000001 --mode stereo with {root}\n"), String::new())
        );
        // a later entry replaces an earlier one of its name, in the list too
        assert_eq!(listed(&scratch.context, &table, &["status"]), (3, String::new(), "another status\n".to_string()));
        let (_, help, _) = listed(&scratch.context, &table, &["help"]);
        assert!(help.contains("\n  play         play catalog ids\n"), "{help}");
        assert!(
            help.contains("\n  status       replaced\n") && !help.contains("saved Apple Music session state"),
            "{help}"
        );
        assert_eq!(help.lines().count(), 2 + commands().len() + 1);
        // an empty table still answers
        assert_eq!(
            listed(&scratch.context, &[], &[]),
            (0, "usage: amui <command> [args]\n\n".to_string(), String::new())
        );
    }

    #[test]
    fn the_doctor_is_a_command() {
        let scratch = Scratch::new();
        let ran = scratch.run(&["doctor"]);
        assert_eq!(ran.status, 1);
        assert!(ran.out.starts_with("\nPrograms\n  MISSING  ffmpeg "), "{}", ran.out);
        assert!(ran.out.ends_with("required item(s) missing. Run ./setup.sh (README.md: Setup).\n"), "{}", ran.out);
        assert_eq!(ran.err, "");
        let ran = scratch.run(&["doctor", "-h"]);
        assert_eq!(ran.status, 0);
        assert!(ran.out.starts_with("usage: amui doctor [-h]\n\nWhat this machine has and lacks"), "{}", ran.out);
        let ran = scratch.run(&["doctor", "now"]);
        assert_eq!((ran.status, ran.out.as_str()), (2, ""));
        assert_eq!(ran.err, "usage: amui doctor [-h]\namui doctor: error: unrecognized arguments: now\n");
    }

    #[test]
    fn every_command_has_help_and_refuses_what_it_does_not_know() {
        let scratch = Scratch::new();
        for command in commands() {
            let ran = scratch.run(&[command.name, "--help"]);
            assert_eq!((ran.status, ran.err.as_str()), (0, ""), "{}", command.name);
            assert!(ran.out.starts_with(&format!("usage: amui {} [-h]", command.name)), "{}", ran.out);
            assert!(ran.out.contains("\noptions:\n  -h, --help"), "{}", ran.out);
            let ran = scratch.run(&[command.name, "--no-such-option"]);
            assert_eq!((ran.status, ran.out.as_str()), (2, ""), "{}", command.name);
            let error = format!("amui {}: error: ", command.name);
            assert!(ran.err.starts_with("usage: amui ") && ran.err.contains(&error), "{}", ran.err);
        }
        // nothing was asked of anybody for that
        assert!(scratch.asked().is_empty() && scratch.wire.lock().unwrap().is_empty());
        assert!(scratch.machine.calls.lock().unwrap().is_empty() && scratch.sink.calls.lock().unwrap().is_empty());
        assert!(scratch.route.asked.lock().unwrap().is_empty());
    }

    #[test]
    fn the_console_keeps_its_two_streams_apart() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut console = Console::new(&mut out, &mut err);
        console.line("a line");
        console.error(String::from("a problem"));
        console.pass("as it is");
        console.pass_error("that too\n");
        assert_eq!(console.stop(&Stop::Help("help text".to_string())), 0);
        assert_eq!(console.stop(&Stop::Usage("usage text".to_string())), 2);
        let _ = console.out().write_all(b"\n");
        assert_eq!(String::from_utf8(out).unwrap(), "a line\nas it ishelp text\n\n");
        assert_eq!(String::from_utf8(err).unwrap(), "a problem\nthat too\nusage text\n");
    }

    /// A reader that went away (a closed pipe) does not stop a command.
    #[test]
    fn a_closed_stream_is_not_an_error() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let scratch = Scratch::new();
        let (mut out, mut err) = (Closed, Closed);
        let status =
            dispatch(&scratch.context, &commands(), &["help".to_string()], &mut Console::new(&mut out, &mut err));
        assert_eq!(status, 0);
        let status =
            dispatch(&scratch.context, &commands(), &["nothing".to_string()], &mut Console::new(&mut out, &mut err));
        assert_eq!(status, 2);
    }

    /// What SIGINT does in this process at the moment.
    fn disposition() -> libc::sighandler_t {
        let mut current = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
        // SAFETY: without a new action sigaction() only writes the current one, through a pointer
        // that is valid for it; all zeros is a value of the struct.
        unsafe {
            assert_eq!(libc::sigaction(libc::SIGINT, std::ptr::null(), current.as_mut_ptr()), 0);
            current.assume_init().sa_sigaction
        }
    }

    /// The real interrupt, with a signal this test sends to itself: the first one is reported
    /// instead of ending the process, and after it the signal means what it always means. A
    /// process that was started with the signal ignored keeps ignoring it.
    #[test]
    fn the_first_ctrl_c_is_reported() {
        let ignored = disposition() == libc::SIG_IGN; // how this test program was started
        assert!(!CtrlC.asked()); // from here on the signal is taken over
        assert_eq!(disposition() == libc::SIG_IGN, ignored);
        // SAFETY: raise() delivers the signal to this thread before it returns; it is ignored, or
        // the handler in place sets a flag.
        assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
        assert_eq!(CtrlC.asked(), !ignored);
        assert_eq!(CtrlC.asked(), !ignored);
        assert_eq!(disposition(), if ignored { libc::SIG_IGN } else { libc::SIG_DFL });
    }
}
