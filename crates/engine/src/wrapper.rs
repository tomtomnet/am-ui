//! The lossless (ALAC) route: control of the local FairPlay wrapper service, and Apple's native
//! sign-in through it. Ported from `tools/lossless/wrapperctl.py`.
//!
//! [`run`] behaves like `python3 wrapperctl.py ARGS`: same subcommands and options, same lines on
//! standard output and standard error, same exit statuses, same files with the same names,
//! formats and permissions, same timeouts. [`main`] is the same thing as the entry of a program:
//! the engine, `setup.sh` and `tools/lossless/*.sh` start the helper as a child process and read
//! what it prints, so that contract is the specification. Everything `run` touches comes from a
//! [`Context`] (standard streams, environment, paths, terminal, clock, process table, signals),
//! so tests run it inside the test process against scratch folders.
//!
//! The service is WorldObservationLog/wrapper "lite" (commit 386ec82) with the 2026-11-05 TLS
//! repair and the am-ui patches (DNS from the host, TLS policy, one answer per credential
//! request), running Apple's Android libraries rootless in a user namespace (no QEMU, no root).
//! It binds 127.0.0.1 only. Subcommands (`tools/lossless/*.sh` are thin wrappers):
//!
//! ```text
//!   paths [--sh]   directories, files and port this helper uses
//!   fingerprint    SHA-256 over the build's own inputs in tools/lossless (build.sh records it)
//!   build-state    two words for ./setup.sh: absent | stale | unstaged | current, then running | stopped
//!   stage          create or refresh the private runtime var/wrapper from the build (account data kept)
//!   start          start the service detached on 127.0.0.1:PORT, pidfile under var/wrapper/run
//!   stop           stop the service this helper started (never another process)
//!   status         health check: process, HTTP /status, signed-in state; prints no secrets
//!   selftest       start the wrapper just far enough to print its usage text: tells whether this system
//!                  lets it run at all (no account, no network); exit status 1 with the reason when not
//!   login          Apple account sign-in, then exit. In a terminal: asks for the email, the password and
//!                  the verification code there. With --pipe (how the am-ui window runs it): the same
//!                  sign-in over JSON lines on stdin/stdout, see login_pipe(). The service must be
//!                  stopped first; --stop-service stops the one this helper started.
//! ```
//!
//! Environment: `AMUI_VAR` (default `<am-ui>/var`), `LOSSLESS_PORT` (default 12351), both through
//! [`Config`]; `LOSSLESS_DNS` (resolver list for the wrapper; default: the host's
//! /etc/resolv.conf), `LOSSLESS_CA_BUNDLE` (PEM bundle copied into the runtime; default: the
//! host's trust store).
//!
//! This helper never reads token files, databases or passwords back from disk. The login keeps
//! the password in the hidden prompt (or the pipe from the am-ui window), in a 0600 argument file
//! inside the private runtime (removed as soon as the wrapper has read it, at the latest when the
//! login ends) and in the wrapper process itself: never in a command line, the environment, a
//! log, an error text or `Debug` output. What the wrapper prints during a `--pipe` login is not
//! passed on verbatim (Apple's dialog texts can name the account); when the attempt fails, its
//! last lines are kept in var/wrapper/logs/signin-last.log (0600) with the Apple ID, e-mail
//! addresses, token-like strings and anything typed into the sign-in left out, and a later
//! success removes them.
//!
//! The wrapper deletes MUSIC_TOKEN and STOREFRONT_ID at the start of every sign-in attempt. To
//! keep a working sign-in when an attempt fails, the login moves both files aside first (a
//! rename, contents unread) and moves them back unless the attempt produced fresh ones.
//!
//! # Differences from the script
//!
//! * Where the script ended with a Python traceback (a file it expected is missing, a manifest
//!   is not JSON, the prompt is closed), this prints one `error: ...` line. The exit status is 1
//!   in both.
//! * Usage errors and `--help` are argparse's lines for an 80-column terminal, with the name in
//!   [`Context::program`] in place of `wrapperctl.py`, without colours and without argparse's
//!   "maybe you meant" hint. Exit statuses 2 and 0 as before.
//! * SIGINT is noticed where the helper waits (reading, sleeping, waiting for a process), not
//!   between any two instructions. A status request in flight (3 seconds at most) is finished
//!   first; the clean-up of a sign-in is never cut short by a second signal.
//! * `stop` force-kills a process of the service only while it is still the same process (pid
//!   and start time), as the end of a sign-in always did. The script looked at the pid alone.
//!   No signal goes to a process number below 2, to the helper itself or to its own process
//!   group, whatever a pidfile says, and a pidfile whose pid is not a number above 1 counts as
//!   stale.
//! * A process that has ended and only waits to be collected by its parent counts as gone. The
//!   script took it for running until then: its `stop` waited for it and could report it as
//!   remaining.
//! * Permissions below the data folder are tightened without following symbolic links: nothing
//!   outside the runtime is ever changed.
//! * A `{"cancel": true}` that arrives in the same read as the credentials cancels the sign-in
//!   before anything is started. The script lost that line and signed in.
//! * The clean-up after a sign-in goes on when one of its steps fails (the script stopped at the
//!   first failing step), and a start check that is interrupted ends its process.
//! * The status request goes to the loopback address directly: no proxy from the environment, no
//!   redirects, at most 1 MiB of answer. Of what a start check prints, the first 1 MiB is looked
//!   at.
//! * `LOSSLESS_PORT` and `AMUI_VAR` are read by [`Config`]: a port that is not a number means
//!   12351 there, where the script ended with a traceback, and an empty `AMUI_VAR` means the
//!   default folder.
//! * Modes of new files and folders do not depend on the umask of the calling process (the
//!   script set 077 for itself; [`main`] does too): files 0600, folders 0700, and the launcher
//!   is started with umask 077.
//! * The helper lock is given back explicitly (`flock` unlock) before its file is closed, so a
//!   copy of the descriptor in a process another thread is just starting cannot hold it.
//! * "Printable" is decided by the `regex` crate's Unicode tables, which are one Unicode version
//!   behind Python's: a character Unicode added last becomes a blank in a message or a log line.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, IsTerminal, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use regex::{NoExpand, Regex};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::util;

pub const HOST: &str = "127.0.0.1";
/// What the launcher is started as (`argv[0]`). It hands its own arguments on to the wrapper, and
/// the wrapper ends without a word when `argv[0]` is 128 characters or longer: so never the full
/// path, which depends on where the am-ui folder lies.
pub const LAUNCHER_ARGV0: &str = "wrapper-lite-rootless";
/// A message of patches/0004, present in a wrapper built with it.
pub const ONE_ANSWER_MARKER: &[u8] = b"requested again, aborting login";
pub const CA_CANDIDATES: [&str; 3] = [
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/ssl/certs/ca-bundle.crt",
];
/// The wrapper appends the six-digit code to the password inside a 64-byte buffer (lite/auth.cpp).
pub const MAX_PASSWORD_BYTES: usize = 63 - 6;
/// Fresh after a successful sign-in.
pub const LOGIN_FILES: [&str; 3] = ["DEV_TOKEN", "MUSIC_TOKEN", "STOREFRONT_ID"];
/// What the wrapper deletes when a sign-in attempt starts.
pub const KEPT_FILES: [&str; 2] = ["MUSIC_TOKEN", "STOREFRONT_ID"];
pub const KEEP_SUFFIX: &str = ".before-login";
// --pipe sign-in: how long each stage may take
/// The window sends the first line right after starting the helper.
pub const PIPE_CREDENTIALS_TIMEOUT_S: f64 = 60.0;
/// The wrapper prints nothing while it is not waiting for the code.
pub const PIPE_QUIET_TIMEOUT_S: f64 = 240.0;
/// Waiting for the person to type the code.
pub const PIPE_CODE_TIMEOUT_S: f64 = 900.0;
/// Where the wrapper can start at all, its usage text is there within a second.
pub const SELFTEST_TIMEOUT_S: f64 = 30.0;
/// The wrapper's last lines of a failed --pipe sign-in that the sign-in log keeps.
pub const SIGNIN_LOG_LINES: usize = 200;
pub const LINE_WITHHELD: &str = "<line left out: it contained something typed into the sign-in>";
/// The answer of the status request is never longer than a few hundred bytes.
const STATUS_MAX_BYTES: u64 = 1 << 20;
/// How much of what the start check prints is looked at.
const SELFTEST_MAX_BYTES: usize = 1 << 20;

/// What `login` prints before it asks for anything (`{data}` is the account data folder).
pub const LOGIN_NOTICE: &str = "\
Apple's native sign-in for am-ui (the sign-in that also makes lossless available)
---------------------------------------------------------------------------------
What runs: Apple's own Android account/FairPlay libraries inside the local wrapper service, in a
rootless user namespace on this machine. They talk to Apple's servers directly over TLS with
certificate verification on (the host CA store). Nothing is sent anywhere else.

What you will be asked, in this terminal only:
  1. your Apple account email (shown as you type)
  2. your password (hidden)
  3. then the wrapper itself prints \"2FA code:\" when Apple asks for the six-digit verification
     code from a trusted device. Type the six digits and press Enter.
     If no code shows up on your devices within a minute, press Ctrl+C here: the attempt ends
     and an earlier sign-in on this machine is kept (research/lossless-route.md has the notes).
     One answer per attempt: a refused password or code ends it, so nothing is retried for you.

Where state goes: {data}
  mpl_db/ (Apple's account database), MUSIC_TOKEN, DEV_TOKEN, STOREFRONT_ID, token_cache.json.
  The directory is private (0700). Treat it like a password; never copy it anywhere.
The password is written to a temporary 0600 file inside that directory for the wrapper to read
and deleted when the login process exits. It never appears in a command line or a log.

Success looks like: \"login successful\" followed by \"login complete, exiting\", then this helper
confirms fresh token files exist (it does not print them). am-ui then uses this sign-in for the
whole app and starts the lossless service when it needs it. If the attempt fails, an earlier
sign-in on this machine is kept.
";

/// What `--help` prints: the script's own description.
const DESCRIPTION: &str = "\
am-ui lossless (ALAC) route: control the local FairPlay wrapper service.

The service is WorldObservationLog/wrapper \"lite\" (commit 386ec82) with the 2026-11-05 TLS repair
and the am-ui patches (DNS from the host, TLS policy, one answer per credential request), running
Apple's Android libraries rootless in a user namespace (no QEMU, no root). It binds 127.0.0.1 only. Subcommands (tools/lossless/*.sh are thin wrappers):

  paths [--sh]   directories, files and port this helper uses
  fingerprint    SHA-256 over the build's own inputs in tools/lossless (build.sh records it)
  build-state    two words for ./setup.sh: absent | stale | unstaged | current, then running | stopped
  stage          create or refresh the private runtime var/wrapper from the build (account data kept)
  start          start the service detached on 127.0.0.1:PORT, pidfile under var/wrapper/run
  stop           stop the service this helper started (never another process)
  status         health check: process, HTTP /status, signed-in state; prints no secrets
  selftest       start the wrapper just far enough to print its usage text: tells whether this system
                 lets it run at all (no account, no network); exit status 1 with the reason when not
  login          Apple account sign-in, then exit. In a terminal: asks for the email, the password and
                 the verification code there. With --pipe (how the am-ui window runs it): the same
                 sign-in over JSON lines on stdin/stdout, see login_pipe(). The service must be
                 stopped first; --stop-service stops the one this helper started.

Environment: AMUI_VAR (default <am-ui>/var), LOSSLESS_PORT (default 12351), LOSSLESS_DNS (resolver
list for the wrapper; default: the host's /etc/resolv.conf), LOSSLESS_CA_BUNDLE (PEM bundle copied
into the runtime; default: the host's trust store).

This helper never reads token files, databases or passwords back from disk. The login keeps the
password in the hidden prompt (or the pipe from the am-ui window), in a 0600 argument file inside
the private runtime (removed as soon as the wrapper has read it, at the latest when the login
process exits) and in the wrapper process itself. What the wrapper prints during a --pipe login is
not passed on verbatim (Apple's dialog texts can name the account); when the attempt fails, its last
lines are kept in var/wrapper/logs/signin-last.log (0600) with the Apple ID, e-mail addresses,
token-like strings and anything typed into the sign-in left out, and a later success removes them.

The wrapper deletes MUSIC_TOKEN and STOREFRONT_ID at the start of every sign-in attempt. To keep a
working sign-in when an attempt fails, the login moves both files aside first (a rename, contents
unread) and moves them back unless the attempt produced fresh ones.";

// ----------------------------------------------------------------------------- errors

/// Why a command ended without doing its work.
#[derive(Debug, thiserror::Error)]
pub enum HelperError {
    /// What the script raised as `HelperError`: printed as `error: ...`, exit status 1.
    #[error("{0}")]
    Refused(String),
    /// The command was told to stop (SIGINT, the script's KeyboardInterrupt): exit status 130.
    #[error("interrupted")]
    Interrupted,
    /// The system refused something the helper needs. The script ended with a traceback here;
    /// exit status 1.
    #[error("{what}: {}", strerror(.error))]
    Io { what: String, error: io::Error },
    /// Something else the script did not expect; `name` is the Python exception it ended with.
    #[error("{message}")]
    Failed { name: &'static str, message: String },
}

impl HelperError {
    fn refused(message: impl Into<String>) -> HelperError {
        HelperError::Refused(message.into())
    }

    fn io(what: impl ToString, error: io::Error) -> HelperError {
        HelperError::Io { what: what.to_string(), error }
    }

    fn failed(name: &'static str, message: impl Into<String>) -> HelperError {
        HelperError::Failed { name, message: message.into() }
    }

    /// The name of the Python exception the script had in this place: a failed `--pipe`
    /// sign-in names it ("The sign-in helper failed (FileNotFoundError)").
    fn python_name(&self) -> &'static str {
        match self {
            HelperError::Refused(_) => "HelperError",
            HelperError::Interrupted => "KeyboardInterrupt",
            HelperError::Io { error, .. } => python_error_name(error),
            HelperError::Failed { name, .. } => name,
        }
    }
}

/// Python's `OSError.strerror`: the system's text for the error, without Rust's "(os error N)".
fn strerror(error: &io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => {
            let text = io::Error::from_raw_os_error(code).to_string();
            match text.rsplit_once(" (os error") {
                Some((message, _)) => message.to_string(),
                None => text,
            }
        }
        None => error.to_string(),
    }
}

/// The subclass of `OSError` Python raises for this error.
fn python_error_name(error: &io::Error) -> &'static str {
    match error.raw_os_error() {
        Some(libc::ENOENT) => "FileNotFoundError",
        Some(libc::EACCES | libc::EPERM) => "PermissionError",
        Some(libc::EEXIST) => "FileExistsError",
        Some(libc::EISDIR) => "IsADirectoryError",
        Some(libc::ENOTDIR) => "NotADirectoryError",
        Some(libc::EINTR) => "InterruptedError",
        Some(libc::EPIPE | libc::ESHUTDOWN) => "BrokenPipeError",
        Some(libc::ECONNREFUSED) => "ConnectionRefusedError",
        Some(libc::ECONNRESET) => "ConnectionResetError",
        Some(libc::ECONNABORTED) => "ConnectionAbortedError",
        Some(libc::ETIMEDOUT) => "TimeoutError",
        Some(libc::EAGAIN | libc::EALREADY | libc::EINPROGRESS) => "BlockingIOError",
        Some(libc::ECHILD) => "ChildProcessError",
        Some(libc::ESRCH) => "ProcessLookupError",
        Some(_) => "OSError",
        None => match error.kind() {
            io::ErrorKind::NotFound => "FileNotFoundError",
            io::ErrorKind::PermissionDenied => "PermissionError",
            io::ErrorKind::AlreadyExists => "FileExistsError",
            io::ErrorKind::InvalidData => "ValueError",
            _ => "OSError",
        },
    }
}

/// A `--pipe` sign-in ended without success: `kind` for the window, `message` safe to show.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct PipeFailure {
    /// input, unavailable, busy, credentials, code, apple, cancelled, timeout or failed.
    pub kind: &'static str,
    pub message: String,
    /// Nothing to look up in the sign-in log.
    pub says_it_all: bool,
    /// The sign-in log written for this failure.
    pub log: Option<String>,
}

impl PipeFailure {
    pub fn new(kind: &'static str, message: impl Into<String>) -> PipeFailure {
        PipeFailure { kind, message: message.into(), says_it_all: false, log: None }
    }

    fn says_it_all(kind: &'static str, message: impl Into<String>) -> PipeFailure {
        PipeFailure { says_it_all: true, ..PipeFailure::new(kind, message) }
    }

    fn cancelled() -> PipeFailure {
        PipeFailure::new("cancelled", "Sign-in cancelled")
    }

    fn code_refused() -> PipeFailure {
        PipeFailure::says_it_all(
            "code",
            "Apple did not accept the verification code. Start again and enter the new code.",
        )
    }

    /// What a failed check before the sign-in means for the window.
    fn from_check(error: HelperError, kind: &'static str) -> PipeFailure {
        match error {
            HelperError::Refused(message) => PipeFailure::new(kind, message),
            HelperError::Interrupted => PipeFailure::cancelled(),
            other => PipeFailure::new("failed", format!("The sign-in helper failed ({})", other.python_name())),
        }
    }
}

/// How a running `--pipe` sign-in stops early: a failure with its own words, or something the
/// helper did not expect (reported like any failed attempt, with the log).
enum PipeStop {
    Failure(PipeFailure),
    Unexpected(&'static str),
}

impl From<PipeFailure> for PipeStop {
    fn from(failure: PipeFailure) -> PipeStop {
        PipeStop::Failure(failure)
    }
}

impl From<HelperError> for PipeStop {
    fn from(error: HelperError) -> PipeStop {
        match error {
            HelperError::Interrupted => PipeStop::Failure(PipeFailure::cancelled()),
            other => PipeStop::Unexpected(other.python_name()),
        }
    }
}

impl From<io::Error> for PipeStop {
    fn from(error: io::Error) -> PipeStop {
        PipeStop::Unexpected(python_error_name(&error))
    }
}

// ----------------------------------------------------------------------------- secrets

/// Something typed into the sign-in: the password, the verification code. It is never printed
/// (`Debug` shows a placeholder), and its bytes are cleared when it is dropped.
struct Secret(String);

impl Secret {
    fn new(text: String) -> Secret {
        Secret(text)
    }

    /// The text itself, for the one place that hands it to the wrapper and for comparisons.
    fn expose(&self) -> &str {
        &self.0
    }

    fn from_utf8(mut bytes: Vec<u8>) -> Result<Secret, HelperError> {
        if std::str::from_utf8(&bytes).is_err() {
            wipe(&mut bytes);
            return Err(HelperError::failed("UnicodeDecodeError", "what was typed is not valid UTF-8"));
        }
        // checked just above; from_utf8() takes the buffer over without copying it
        String::from_utf8(bytes)
            .map(Secret)
            .map_err(|_| HelperError::failed("UnicodeDecodeError", "what was typed is not valid UTF-8"))
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<hidden>)")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // SAFETY: zero bytes are valid UTF-8
        wipe(unsafe { self.0.as_mut_vec() });
    }
}

/// Overwrite a buffer that held something secret.
fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: a valid, exclusive reference; the volatile write keeps the compiler from dropping it
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
}

// ----------------------------------------------------------------------------- seams

/// The time as the helper sees it.
pub trait Clock: Send + Sync {
    /// `time.monotonic()`: a clock that never goes back; deadlines are measured on it.
    fn monotonic(&self) -> Duration;

    /// `time.time()`: the wall clock, for time stamps and for "written since the sign-in began".
    fn now(&self) -> SystemTime;
}

/// The clocks of this computer.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

static CLOCK_START: LazyLock<Instant> = LazyLock::new(Instant::now);

impl Clock for SystemClock {
    fn monotonic(&self) -> Duration {
        CLOCK_START.elapsed()
    }

    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// One start of the launcher: what `subprocess.Popen` got in the script.
#[derive(Debug)]
pub struct Launch {
    /// The launcher file.
    pub program: PathBuf,
    /// Its argument list. The first entry is the name it runs under ([`LAUNCHER_ARGV0`]), never
    /// the path.
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// The whole environment of the launcher.
    pub env: Vec<(String, String)>,
    pub stdin: Stdio,
    pub stdout: Stdio,
    pub stderr: Stdio,
    /// In its own session: the service, the `--pipe` sign-in and the start check.
    pub new_session: bool,
}

/// What the helper asks the operating system: the process table as /proc shows it, the settings
/// that can forbid user namespaces, and the start of the launcher. Tests answer in its place.
pub trait System: Send + Sync {
    /// Field 22 of `/proc/<pid>/stat` (start time in clock ticks) or `None` if the process is gone.
    /// One that has ended and only waits to be collected by its parent is gone.
    fn proc_start_time(&self, pid: i32) -> Option<u64>;

    /// The argument list of a process; empty when it cannot be read.
    fn proc_cmdline(&self, pid: i32) -> Vec<String>;

    /// The program a process runs: the target of `/proc/<pid>/exe`.
    fn proc_program(&self, pid: i32) -> io::Result<String>;

    /// The children of a process, over all its threads.
    fn proc_children(&self, pid: i32) -> Vec<i32>;

    /// Every process there is.
    fn pids(&self) -> Vec<i32>;

    /// A number under /proc/sys ("kernel/unprivileged_userns_clone"), `None` when there is none.
    fn sysctl(&self, name: &str) -> Option<i64>;

    /// Start the launcher.
    fn spawn(&self, launch: Launch) -> io::Result<Child>;

    /// Host-namespace PIDs of every descendant (the wrapper's child runs as PID 1 of its own
    /// namespace but is visible here).
    fn descendants(&self, pid: i32) -> Vec<i32> {
        let mut found = Vec::new();
        let mut pending = vec![pid];
        while let Some(current) = pending.pop() {
            for child in self.proc_children(current) {
                if !found.contains(&child) {
                    found.push(child);
                    pending.push(child);
                }
            }
        }
        found
    }
}

/// This computer.
#[derive(Debug, Default, Clone, Copy)]
pub struct Host;

impl System for Host {
    fn proc_start_time(&self, pid: i32) -> Option<u64> {
        let stat = fs::read(format!("/proc/{pid}/stat")).ok()?;
        let stat = String::from_utf8_lossy(&stat);
        // The command name may contain spaces or parentheses; split after the last ')'.
        let mut fields = stat.get(stat.rfind(')')? + 2..)?.split_whitespace();
        // Field 3, the state: a process that has ended and only waits to be collected is gone.
        if matches!(fields.next()?, "Z" | "X" | "x") {
            return None;
        }
        fields.nth(18)?.parse().ok()
    }

    fn proc_cmdline(&self, pid: i32) -> Vec<String> {
        let Ok(raw) = fs::read(format!("/proc/{pid}/cmdline")) else {
            return Vec::new();
        };
        raw.split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect()
    }

    fn proc_program(&self, pid: i32) -> io::Result<String> {
        Ok(fs::read_link(format!("/proc/{pid}/exe"))?.to_string_lossy().into_owned())
    }

    fn proc_children(&self, pid: i32) -> Vec<i32> {
        let mut found = Vec::new();
        let Ok(threads) = fs::read_dir(format!("/proc/{pid}/task")) else {
            return found;
        };
        for thread in threads.flatten() {
            if let Ok(children) = fs::read_to_string(thread.path().join("children")) {
                found.extend(children.split_whitespace().filter_map(|child| child.parse::<i32>().ok()));
            }
        }
        found
    }

    fn pids(&self) -> Vec<i32> {
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries.flatten().filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok()).collect()
    }

    fn sysctl(&self, name: &str) -> Option<i64> {
        fs::read_to_string(Path::new("/proc/sys").join(name)).ok()?.trim().parse().ok()
    }

    fn spawn(&self, launch: Launch) -> io::Result<Child> {
        let Launch { program, argv, cwd, env, stdin, stdout, stderr, new_session } = launch;
        let mut command = Command::new(program);
        if let Some((name, arguments)) = argv.split_first() {
            command.arg0(name).args(arguments);
        }
        command.current_dir(cwd).env_clear().envs(env).stdin(stdin).stdout(stdout).stderr(stderr);
        // SAFETY: setsid() and umask() are async-signal-safe, and the closure does nothing else
        // between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if new_session && libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                libc::umask(0o077);
                Ok(())
            });
        }
        command.spawn()
    }
}

/// How a command learns that it was asked to stop: SIGINT (the script's KeyboardInterrupt), and
/// for `login --pipe` also SIGTERM and SIGHUP, which end the sign-in like a cancel. A flag that
/// can be waited for together with file descriptors; once raised it stays raised.
pub struct Interrupt {
    reader: OwnedFd,
    writer: OwnedFd,
    signal: AtomicI32,
    /// The one the signal handlers of this process report to.
    from_signals: bool,
}

/// Where the signal handler reports: the [`Interrupt`] of [`Interrupt::for_process`].
static SIGNAL_TARGET: AtomicPtr<Interrupt> = AtomicPtr::new(std::ptr::null_mut());

extern "C" fn on_signal(signal: libc::c_int) {
    // SAFETY: only async-signal-safe calls (an atomic load, write()), with errno put back; the
    // target is never released once it has been stored.
    unsafe {
        let errno = *libc::__errno_location();
        let target = SIGNAL_TARGET.load(Ordering::SeqCst);
        if !target.is_null() {
            (*target).raise(signal);
        }
        *libc::__errno_location() = errno;
    }
}

fn install_handler(signal: libc::c_int) {
    // SAFETY: a zeroed sigaction is valid; the handler is async-signal-safe (see on_signal).
    // SA_RESTART: system calls in progress go on, the waits of this module wake through the pipe.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(signal, &action, std::ptr::null_mut());
    }
}

fn signal_ignored(signal: libc::c_int) -> bool {
    // SAFETY: reads the current disposition into a zeroed sigaction.
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        libc::sigaction(signal, std::ptr::null(), &mut current) == 0 && current.sa_sigaction == libc::SIG_IGN
    }
}

impl Interrupt {
    /// A flag nothing raises by itself: [`Interrupt::raise`] does.
    pub fn new() -> io::Result<Interrupt> {
        let mut fds: [RawFd; 2] = [-1; 2];
        // SAFETY: pipe2() fills the two descriptors, which nobody else owns.
        let (reader, writer) = unsafe {
            if libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) != 0 {
                return Err(io::Error::last_os_error());
            }
            (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
        };
        Ok(Interrupt { reader, writer, signal: AtomicI32::new(0), from_signals: false })
    }

    /// The flag of a helper that is a program of its own: SIGINT raises it, unless the signal
    /// was ignored when the program started (Python leaves it ignored too).
    pub fn for_process() -> io::Result<Arc<Interrupt>> {
        let interrupt = Arc::new(Interrupt { from_signals: true, ..Interrupt::new()? });
        // The handler uses it for the rest of the process: this reference is never given back.
        SIGNAL_TARGET.store(Arc::into_raw(interrupt.clone()).cast_mut(), Ordering::SeqCst);
        if !signal_ignored(libc::SIGINT) {
            install_handler(libc::SIGINT);
        }
        Ok(interrupt)
    }

    /// From now on these signals raise the flag too (only for [`Interrupt::for_process`]).
    pub fn watch(&self, signals: &[i32]) {
        if self.from_signals {
            for signal in signals {
                install_handler(*signal);
            }
        }
    }

    /// Ask the command to stop. This is what the signal handler does, so it only stores the
    /// signal and writes one byte; tests call it in place of a signal.
    pub fn raise(&self, signal: i32) {
        let _ = self.signal.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
        // SAFETY: writes one byte from a live buffer; a full pipe already says "raised".
        unsafe { libc::write(self.writer.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
    }

    /// The first signal that asked the command to stop.
    pub fn raised(&self) -> Option<i32> {
        match self.signal.load(Ordering::SeqCst) {
            0 => None,
            signal => Some(signal),
        }
    }
}

/// Every folder and file the helper uses: the script's module constants.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The am-ui folder.
    pub root: PathBuf,
    /// tools/lossless: the inputs of the build (`build.sh`, the pinned hashes, the patches).
    pub here: PathBuf,
    pub var: PathBuf,
    /// var/build/lossless.
    pub build: PathBuf,
    /// var/wrapper: the private runtime.
    pub runtime: PathBuf,
    pub rootfs: PathBuf,
    /// The wrapper's account state.
    pub data: PathBuf,
    pub run: PathBuf,
    pub logs: PathBuf,
    pub pidfile: PathBuf,
    pub lockfile: PathBuf,
    pub launcher: PathBuf,
    pub built_lite: PathBuf,
    pub built_system: PathBuf,
    pub pin_file: PathBuf,
    /// The wrapper's last lines of a failed `--pipe` sign-in, scrubbed.
    pub signin_log: PathBuf,
    /// The host's resolver list.
    pub resolv_conf: PathBuf,
    /// Where the host's trust store may be.
    pub ca_candidates: Vec<PathBuf>,
}

impl Paths {
    pub fn new(config: &Config) -> Paths {
        let root = clean(&config.root);
        let here = root.join("tools/lossless");
        let var = clean(&config.var);
        let build = clean(&config.lossless_build_dir);
        let runtime = var.join("wrapper");
        let rootfs = runtime.join("rootfs");
        let run = runtime.join("run");
        let logs = runtime.join("logs");
        Paths {
            data: clean(&config.lossless_data_dir),
            pidfile: run.join("service.pid.json"),
            lockfile: run.join("helper.lock"),
            launcher: build.join("bin").join(LAUNCHER_ARGV0),
            built_lite: build.join("wrapper-src/rootfs/system/bin/lite"),
            built_system: build.join("wrapper-src/rootfs/system"),
            pin_file: here.join("fairplay-libs.sha256"),
            signin_log: logs.join("signin-last.log"),
            resolv_conf: PathBuf::from("/etc/resolv.conf"),
            ca_candidates: CA_CANDIDATES.iter().map(PathBuf::from).collect(),
            root,
            here,
            var,
            build,
            runtime,
            rootfs,
            run,
            logs,
        }
    }
}

/// A path as Python's `pathlib` spells it: no "." parts, no doubled or trailing separators.
fn clean(path: &Path) -> PathBuf {
    let cleaned: PathBuf = path.components().filter(|part| !matches!(part, Component::CurDir)).collect();
    if cleaned.as_os_str().is_empty() { PathBuf::from(".") } else { cleaned }
}

/// Everything a command touches outside its own code.
pub struct Context {
    pub paths: Paths,
    /// The port of the service on 127.0.0.1 (`LOSSLESS_PORT`).
    pub port: u16,
    /// The environment; `LOSSLESS_DNS` and `LOSSLESS_CA_BUNDLE` are read from it.
    pub env: HashMap<String, String>,
    pub stdin: OwnedFd,
    pub stdout: OwnedFd,
    pub stderr: OwnedFd,
    /// "Is this a terminal": what the system says about `stdin` and `stdout` unless a test says
    /// otherwise. The terminal sign-in needs both, `--pipe` refuses a terminal.
    pub stdin_is_terminal: bool,
    pub stdout_is_terminal: bool,
    /// Where the hidden prompt goes first: the controlling terminal ("/dev/tty"). `None`, or a
    /// path that cannot be opened: standard input, with the prompt on standard error.
    pub controlling_terminal: Option<PathBuf>,
    pub clock: Arc<dyn Clock>,
    pub system: Arc<dyn System>,
    pub interrupt: Arc<Interrupt>,
    /// The name in usage lines.
    pub program: String,
}

impl Context {
    /// A helper over `config` with the standard streams given: for tests and for callers that
    /// run a command inside their own process. No controlling terminal, no signals.
    pub fn new(
        config: &Config,
        env: &HashMap<String, String>,
        stdin: OwnedFd,
        stdout: OwnedFd,
        stderr: OwnedFd,
    ) -> io::Result<Context> {
        Ok(Context {
            paths: Paths::new(config),
            port: config.lossless_port,
            env: env.clone(),
            stdin_is_terminal: stdin.is_terminal(),
            stdout_is_terminal: stdout.is_terminal(),
            stdin,
            stdout,
            stderr,
            controlling_terminal: None,
            clock: Arc::new(SystemClock),
            system: Arc::new(Host),
            interrupt: Arc::new(Interrupt::new()?),
            program: "wrapperctl".to_string(),
        })
    }

    /// The helper as a program of its own: the configuration and environment of this process,
    /// its standard streams, its controlling terminal, and SIGINT as the interruption.
    pub fn from_process() -> io::Result<Context> {
        let config = Config::from_process();
        let env: HashMap<String, String> = std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
            .collect();
        let stream = |fd: RawFd| -> io::Result<OwnedFd> {
            // SAFETY: fcntl() on a descriptor number (a closed one is an error, not a fault); the
            // copy it returns is a new descriptor nobody else owns.
            unsafe {
                let copy = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3);
                if copy >= 0 {
                    return Ok(OwnedFd::from_raw_fd(copy));
                }
            }
            // a program started with a standard stream closed
            Ok(OpenOptions::new().read(true).write(true).open("/dev/null")?.into())
        };
        let mut context = Context::new(&config, &env, stream(0)?, stream(1)?, stream(2)?)?;
        context.controlling_terminal = Some(PathBuf::from("/dev/tty"));
        context.interrupt = Interrupt::for_process()?;
        Ok(context)
    }

    pub fn status_url(&self) -> String {
        format!("http://{HOST}:{}/status", self.port)
    }
}

// ----------------------------------------------------------------------------- what Python does

/// `str.isspace()` for one character.
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `str.strip()`.
fn py_strip(text: &str) -> &str {
    text.trim_matches(is_py_space)
}

/// `str.split()`.
fn py_split(text: &str) -> impl Iterator<Item = &str> {
    text.split(is_py_space).filter(|part| !part.is_empty())
}

/// `str.splitlines()`.
fn py_splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        if !matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}') {
            continue;
        }
        lines.push(&text[start..index]);
        start = index + c.len_utf8();
        if c == '\r' && matches!(chars.peek(), Some((_, '\n'))) {
            chars.next();
            start += 1;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// `bytes.strip()`: the six ASCII whitespace bytes.
fn is_py_space_byte(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// What `str.isprintable()` rejects: the "Other" and "Separator" categories, except the space.
static NONPRINTABLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[[\p{C}\p{Z}]--[ ]]").expect("a valid pattern"));

/// `text` with every character that is not printable replaced by a space.
fn printable_only(text: &str) -> String {
    NONPRINTABLE.replace_all(text, " ").into_owned()
}

fn is_printable(c: char) -> bool {
    !NONPRINTABLE.is_match(c.encode_utf8(&mut [0; 4]))
}

/// One line without control characters, runs of blanks as one space, at most `limit` characters.
pub fn printable(text: &str, limit: usize) -> String {
    py_split(&printable_only(text)).collect::<Vec<_>>().join(" ").chars().take(limit).collect()
}

/// `repr()` of a string.
fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
    let mut out = String::from(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if c == ' ' || is_printable(c) => out.push(c),
            c if (c as u32) < 0x100 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push_str(&format!("\\U{:08x}", c as u32)),
        }
    }
    out.push(quote);
    out
}

/// `repr()` of a float: the shortest digits that read back as the same number, in Python's
/// notation.
fn py_float_repr(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let sign = if mantissa.starts_with('-') { "-" } else { "" };
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    if (-4..16).contains(&exponent) {
        if exponent < 0 {
            return format!("{sign}0.{}{digits}", "0".repeat((-exponent - 1) as usize));
        }
        let whole = exponent as usize + 1;
        if digits.len() <= whole {
            return format!("{sign}{digits}{}.0", "0".repeat(whole - digits.len()));
        }
        return format!("{sign}{}.{}", &digits[..whole], &digits[whole..]);
    }
    let (first, rest) = digits.split_at(1.min(digits.len()));
    let fraction = if rest.is_empty() { String::new() } else { format!(".{rest}") };
    format!("{sign}{first}{fraction}e{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.abs())
}

/// `json.dumps()` as the script calls it: Python's separators and escapes, keys in the order
/// they were inserted. `indent` is `None` for one line, `Some(2)` for `indent=2`.
fn json_dumps(value: &Value, indent: Option<usize>) -> String {
    let mut out = String::new();
    json_write(value, indent, 0, &mut out);
    out
}

fn json_write(value: &Value, indent: Option<usize>, level: usize, out: &mut String) {
    let open = |out: &mut String, level: usize| {
        if let Some(width) = indent {
            out.push('\n');
            out.push_str(&" ".repeat(width * level));
        }
    };
    let separator = if indent.is_some() { "," } else { ", " };
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => match number.as_f64() {
            Some(float) if number.is_f64() => out.push_str(&py_float_repr(float)),
            _ => out.push_str(&number.to_string()),
        },
        Value::String(text) => json_string(text, out),
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(separator);
                }
                open(out, level + 1);
                json_write(item, indent, level + 1, out);
            }
            open(out, level);
            out.push(']');
        }
        Value::Object(members) if members.is_empty() => out.push_str("{}"),
        Value::Object(members) => {
            out.push('{');
            for (index, (key, member)) in members.iter().enumerate() {
                if index > 0 {
                    out.push_str(separator);
                }
                open(out, level + 1);
                json_string(key, out);
                out.push_str(": ");
                json_write(member, indent, level + 1, out);
            }
            open(out, level);
            out.push('}');
        }
    }
}

/// A JSON string with `ensure_ascii`: everything outside printable ASCII is escaped.
fn json_string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                for unit in c.encode_utf16(&mut [0; 2]) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// Python's truth value of a JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64() != Some(0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(members)) => !members.is_empty(),
    }
}

/// `str(ipaddress.ip_address(text))`, `None` for anything that is not an address.
fn ip_address(text: &str) -> Option<String> {
    if let Ok(address) = text.parse::<Ipv4Addr>() {
        return Some(address.to_string());
    }
    match text.split_once('%') {
        None => text.parse::<Ipv6Addr>().ok().map(|address| address.to_string()),
        Some((address, scope)) if !scope.is_empty() && !scope.contains('%') => {
            address.parse::<Ipv6Addr>().ok().map(|address| format!("{address}%{scope}"))
        }
        Some(_) => None,
    }
}

/// `os.path.realpath()`: symbolic links resolved as far as the path exists.
fn realpath(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut missing = Vec::new();
    let mut existing = absolute.as_path();
    loop {
        if let Ok(resolved) = existing.canonicalize() {
            return missing.iter().rev().fold(resolved, |path, name| path.join(name));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                existing = parent;
            }
            _ => return absolute,
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => return Ok(hex(&digest.finalize())),
            Ok(count) => digest.update(&chunk[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn broken_down(now: SystemTime, local: bool) -> libc::tm {
    let seconds = match now.duration_since(UNIX_EPOCH) {
        Ok(since) => since.as_secs() as libc::time_t,
        Err(before) => -(before.duration().as_secs() as libc::time_t),
    };
    // SAFETY: a zeroed tm is valid, and both functions only write into it.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if local {
            libc::localtime_r(&seconds, &mut tm);
        } else {
            libc::gmtime_r(&seconds, &mut tm);
        }
        tm
    }
}

fn date_and_time(tm: &libc::tm) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// `time.strftime("%Y-%m-%dT%H:%M:%S%z")`: local time with its distance from UTC.
fn local_stamp(now: SystemTime) -> String {
    let tm = broken_down(now, true);
    let (sign, offset) = if tm.tm_gmtoff < 0 { ('-', -tm.tm_gmtoff) } else { ('+', tm.tm_gmtoff) };
    format!("{}{sign}{:02}{:02}", date_and_time(&tm), offset / 3600, offset % 3600 / 60)
}

/// `time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())`.
fn utc_stamp(now: SystemTime) -> String {
    format!("{}Z", date_and_time(&broken_down(now, false)))
}

// ----------------------------------------------------------------------------- descriptors

fn write_all(fd: BorrowedFd<'_>, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        // SAFETY: writes from a live buffer of the given length.
        let written = unsafe { libc::write(fd.as_raw_fd(), data.as_ptr().cast(), data.len()) };
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(error);
        }
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        data = &data[written as usize..];
    }
    Ok(())
}

/// One `read()`; 0 at the end of the input.
fn read_some(fd: BorrowedFd<'_>, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        // SAFETY: reads into a live buffer of the given length.
        let count = unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if count >= 0 {
            return Ok(count as usize);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
}

/// A pseudo-terminal: (master, slave), both closed in programs this process starts.
fn open_pty() -> io::Result<(OwnedFd, OwnedFd)> {
    // SAFETY: plain libc calls on descriptors this function owns; the name buffer is large
    // enough for any /dev/pts path and is NUL-terminated by ptsname_r().
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        if master < 0 {
            return Err(io::Error::last_os_error());
        }
        let master = OwnedFd::from_raw_fd(master);
        let mut name = [0 as libc::c_char; 128];
        if libc::grantpt(master.as_raw_fd()) != 0
            || libc::unlockpt(master.as_raw_fd()) != 0
            || libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) != 0
        {
            return Err(io::Error::last_os_error());
        }
        let slave = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        if slave < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((master, OwnedFd::from_raw_fd(slave)))
    }
}

fn terminal_attributes(fd: BorrowedFd<'_>) -> io::Result<libc::termios> {
    // SAFETY: a zeroed termios is valid, and tcgetattr() only writes into it.
    unsafe {
        let mut attributes: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd.as_raw_fd(), &mut attributes) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(attributes)
    }
}

fn set_terminal_attributes(fd: BorrowedFd<'_>, when: libc::c_int, attributes: &libc::termios) -> io::Result<()> {
    // SAFETY: passes a valid termios.
    if unsafe { libc::tcsetattr(fd.as_raw_fd(), when, attributes) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Puts the terminal back the way the hidden prompt found it.
struct EchoBack<'f> {
    fd: BorrowedFd<'f>,
    saved: libc::termios,
}

impl Drop for EchoBack<'_> {
    fn drop(&mut self) {
        let _ = set_terminal_attributes(self.fd, libc::TCSAFLUSH, &self.saved);
    }
}

/// Send a signal to one process. Never to a group, to "every process", to init or to the
/// helper itself, whatever a file said the number was.
fn signal_process(pid: i32, signal: libc::c_int) -> bool {
    // SAFETY: kill() with a positive process id.
    pid > 1 && pid as u32 != std::process::id() && unsafe { libc::kill(pid, signal) } == 0
}

/// Send a signal to the process group a launcher of this helper leads: its own, since it was
/// started in a session of its own. Never to the group the helper itself is in.
fn signal_group(pgid: i32, signal: libc::c_int) -> bool {
    // SAFETY: getpgrp() has no preconditions; killpg() with a positive group id.
    pgid > 1 && pgid != unsafe { libc::getpgrp() } && unsafe { libc::killpg(pgid, signal) } == 0
}

/// Let a process this helper started go on by itself: when it ends it is collected, so it does
/// not stay behind as a defunct entry of a calling process that lives on.
fn detach(mut child: Child) {
    if !matches!(child.try_wait(), Ok(Some(_))) {
        let _ = std::thread::Builder::new().name("amui-wrapper-wait".into()).spawn(move || {
            let _ = child.wait();
        });
    }
}

/// `Popen.returncode`: the exit status, or the negative number of the signal that ended it.
fn return_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| -status.signal().unwrap_or(0))
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// `Path.mkdir(exist_ok=True)`: one level, mode 0700 (what the script's umask left of its modes).
fn make_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Err(error) if !(error.kind() == io::ErrorKind::AlreadyExists && path.is_dir()) => Err(error),
        _ => Ok(()),
    }
}

/// A new or truncated file of the owner alone (an existing file keeps its mode).
fn create_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)
}

fn copy_if_changed(src: &Path, dst: &Path, mode: u32) -> io::Result<bool> {
    if dst.is_file() && fs::metadata(dst)?.len() == fs::metadata(src)?.len() && sha256_file(dst)? == sha256_file(src)? {
        fs::set_permissions(dst, Permissions::from_mode(mode))?;
        return Ok(false);
    }
    let mut name = dst.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = dst.with_file_name(name);
    let mut source = File::open(src)?;
    let mut target = create_file(&tmp)?;
    io::copy(&mut source, &mut target)?;
    drop(target);
    fs::set_permissions(&tmp, Permissions::from_mode(mode))?;
    fs::rename(&tmp, dst)?;
    Ok(true)
}

/// `bytes.count()`: how often `needle` occurs, occurrences not overlapping.
fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    let Some((first, rest)) = needle.split_first() else {
        return 0;
    };
    let mut count = 0;
    let mut from = 0;
    // by the first byte: the wrapper's program is some megabytes
    while let Some(offset) = haystack[from..].iter().position(|byte| byte == first) {
        let at = from + offset;
        if haystack[at + 1..].starts_with(rest) {
            count += 1;
            from = at + needle.len();
        } else {
            from = at + 1;
        }
    }
    count
}

/// The wrapper's /status answer: its storefront list, `None` when nothing answers like the
/// wrapper.
fn status_regions(url: &str, timeout: Duration) -> Option<Vec<Value>> {
    let limit = Some(timeout);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .http_status_as_error(false)
        .timeout_connect(limit)
        .timeout_send_request(limit)
        .timeout_recv_response(limit)
        .timeout_recv_body(limit)
        .build()
        .into();
    let mut response = agent.get(url).header("Connection", "close").call().ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = response.body_mut().with_config().limit(STATUS_MAX_BYTES).read_to_vec().ok()?;
    let payload: Value = serde_json::from_slice(&body).ok()?;
    let payload = payload.as_object()?;
    // Python: `payload.get("code") != 0`
    match payload.get("code")? {
        Value::Number(code) if code.as_f64() == Some(0.0) => {}
        Value::Bool(false) => {}
        _ => return None,
    }
    payload.get("data")?.as_object()?.get("regions")?.as_array().cloned()
}

// ----------------------------------------------------------------------------- command line

const COMMANDS: [&str; 9] =
    ["paths", "fingerprint", "build-state", "stage", "start", "stop", "status", "selftest", "login"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Invocation {
    Paths { sh: bool },
    Fingerprint,
    BuildState,
    Stage,
    Start,
    Stop,
    Status,
    Selftest,
    Login { explain_only: bool, pipe: bool, stop_service: bool },
}

#[derive(Debug, PartialEq, Eq)]
enum Parsed {
    Run(Invocation),
    /// argparse ended the program: help on standard output, or a usage error on standard error.
    Exit {
        status: i32,
        stdout: String,
        stderr: String,
    },
}

fn usage(program: &str) -> String {
    let indent = " ".repeat("usage: ".len() + program.len() + 1);
    format!("usage: {program} [-h]\n{indent}{{{}}} ...\n", COMMANDS.join(","))
}

fn command_flags(command: &str) -> &'static [&'static str] {
    match command {
        "paths" => &["--sh"],
        "login" => &["--explain-only", "--pipe", "--stop-service"],
        _ => &[],
    }
}

fn command_usage(program: &str, command: &str) -> String {
    let flags: String = command_flags(command).iter().map(|flag| format!(" [{flag}]")).collect();
    format!("usage: {program} {command} [-h]{flags}\n")
}

fn command_help(program: &str, command: &str) -> String {
    let options = match command {
        "paths" => "  -h, --help  show this help message and exit\n  --sh        print shell export lines\n",
        "login" => concat!(
            "  -h, --help      show this help message and exit\n",
            "  --explain-only  print the notice and exit\n",
            "  --pipe          sign in over JSON lines on stdin/stdout (how the am-ui\n",
            "                  window runs it)\n",
            "  --stop-service  stop the service this helper started before signing in\n",
        ),
        _ => "  -h, --help  show this help message and exit\n",
    };
    format!("{}\noptions:\n{options}", command_usage(program, command))
}

fn usage_error(usage: String, program: &str, message: &str) -> Parsed {
    Parsed::Exit { status: 2, stdout: String::new(), stderr: format!("{usage}{program}: error: {message}\n") }
}

fn help(text: String) -> Parsed {
    Parsed::Exit { status: 0, stdout: text, stderr: String::new() }
}

/// The command line as argparse reads it: long options may be shortened, `-h` anywhere prints
/// help, what is left over is an error.
fn parse_args(args: &[String], program: &str) -> Parsed {
    let mut unrecognized: Vec<&str> = Vec::new();
    let mut only_positional = false;
    for (index, arg) in args.iter().enumerate() {
        let arg = arg.as_str();
        if !only_positional && arg == "--" {
            only_positional = true;
            continue;
        }
        if !only_positional && arg.starts_with('-') && arg != "-" {
            let name = arg.split_once('=').map_or(arg, |(name, _)| name);
            if arg == "-h" || (name.len() > 2 && "--help".starts_with(name)) {
                if let Some((_, value)) = arg.split_once('=') {
                    let message = format!("argument -h/--help: ignored explicit argument {}", py_repr(value));
                    return usage_error(usage(program), program, &message);
                }
                let choices = COMMANDS.join(",");
                return help(format!(
                    "{}\n{DESCRIPTION}\n\npositional arguments:\n  {{{choices}}}\n\noptions:\n  -h, --help            \
                     show this help message and exit\n",
                    usage(program)
                ));
            }
            unrecognized.push(arg);
            continue;
        }
        if !COMMANDS.contains(&arg) {
            let choices: Vec<String> = COMMANDS.iter().map(|command| py_repr(command)).collect();
            let message =
                format!("argument command: invalid choice: {} (choose from {})", py_repr(arg), choices.join(", "));
            return usage_error(usage(program), program, &message);
        }
        return parse_command(arg, &args[index + 1..], unrecognized, program);
    }
    usage_error(usage(program), program, "the following arguments are required: command")
}

fn parse_command<'a>(command: &str, args: &'a [String], mut unrecognized: Vec<&'a str>, program: &str) -> Parsed {
    let flags = command_flags(command);
    let mut given: Vec<&str> = Vec::new();
    let mut rest = args.iter().map(String::as_str);
    while let Some(arg) = rest.next() {
        if arg == "--" {
            unrecognized.push(arg);
            unrecognized.extend(rest);
            break;
        }
        if arg == "-h" {
            return help(command_help(program, command));
        }
        if !arg.starts_with("--") {
            unrecognized.push(arg);
            continue;
        }
        let (name, value) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (arg, None),
        };
        let known = ["--help"].iter().chain(flags);
        let matches: Vec<&str> = match known.clone().find(|option| **option == name) {
            Some(exact) => vec![*exact],
            None => known.filter(|option| option.starts_with(name)).copied().collect(),
        };
        let sub_program = format!("{program} {command}");
        match (matches.as_slice(), value) {
            ([], _) => unrecognized.push(arg),
            ([option], Some(value)) => {
                let shown = if *option == "--help" { "-h/--help" } else { option };
                let message = format!("argument {shown}: ignored explicit argument {}", py_repr(value));
                return usage_error(command_usage(program, command), &sub_program, &message);
            }
            (["--help"], None) => return help(command_help(program, command)),
            ([option], None) => given.push(option),
            (several, _) => {
                let message = format!("ambiguous option: {arg} could match {}", several.join(", "));
                return usage_error(command_usage(program, command), &sub_program, &message);
            }
        }
    }
    if !unrecognized.is_empty() {
        let message = format!("unrecognized arguments: {}", unrecognized.join(" "));
        return usage_error(usage(program), program, &message);
    }
    let has = |flag: &str| given.contains(&flag);
    Parsed::Run(match command {
        "paths" => Invocation::Paths { sh: has("--sh") },
        "fingerprint" => Invocation::Fingerprint,
        "build-state" => Invocation::BuildState,
        "stage" => Invocation::Stage,
        "start" => Invocation::Start,
        "stop" => Invocation::Stop,
        "status" => Invocation::Status,
        "selftest" => Invocation::Selftest,
        _ => Invocation::Login {
            explain_only: has("--explain-only"),
            pipe: has("--pipe"),
            stop_service: has("--stop-service"),
        },
    })
}

// ----------------------------------------------------------------------------- sign-in texts

/// The limits of the wrapper's argument format and password buffer.
pub fn check_credentials(username: &str, password: &str) -> Result<(), HelperError> {
    if username.is_empty() || password.is_empty() {
        return Err(HelperError::refused("an email and a password are required"));
    }
    if username.contains([':', ' ', '\t', '\r', '\n', '\0']) {
        return Err(HelperError::refused("the wrapper's argument format cannot carry this email address"));
    }
    if password.contains(['\r', '\n', '\0']) || password != password.trim_end_matches([' ', '\t']) {
        return Err(HelperError::refused(
            "the wrapper's argument format cannot preserve line breaks or trailing spaces in this password",
        ));
    }
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(HelperError::refused(format!(
            "this wrapper keeps password plus 2FA code in a 63-byte buffer; passwords longer than \
             {MAX_PASSWORD_BYTES} bytes cannot be used with it"
        )));
    }
    Ok(())
}

static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\S+@\S+").expect("a valid pattern"));
static ANSI_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").expect("a valid pattern"));
static TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z0-9+/=_-]{32,}").expect("a valid pattern"));
static NUMBER_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d{8,}").expect("a valid pattern"));
static DIALOG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"dialogHandler: \{title: (.*?), message: (.*)").expect("a valid pattern"));
static AUTH_ERROR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"auth error: code=(-?\d+)(?:, message=(.*))?").expect("a valid pattern"));
static RESPONSE_TYPE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"auth failed: response type (-?\d+)").expect("a valid pattern"));
/// A library the Android loader misses whose name carries a version ("libcurl.so.4"): Android
/// libraries have none, so the wrapper was linked against a library of the system it was built
/// on.
static FOREIGN_LIBRARY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"library "([^"/]+\.so\.[0-9][^"/]*)" not found"#).expect("a valid pattern"));
/// The launcher's own steps before the wrapper runs (the names its perror() lines start with).
const NAMESPACE_STEPS: [&str; 6] = ["unshare", "uid_map", "setgroups", "gid_map", "mount ", "chroot "];
const FOLDER_STEPS: [&str; 3] = ["mkdir ", "open ", "chdir "];

/// `text` with every spelling of the account name replaced, whatever its case.
fn without_account(text: &str, username: &str, replacement: &str) -> String {
    if username.is_empty() {
        return text.to_string();
    }
    match Regex::new(&format!("(?i){}", regex::escape(username))) {
        Ok(pattern) => pattern.replace_all(text, NoExpand(replacement)).into_owned(),
        Err(_) => text.replace(username, replacement),
    }
}

/// Apple's own sentence about a refused sign-in, for display only: one line, no account name.
pub fn safe_apple_message(text: &str, username: &str) -> String {
    let text = without_account(&printable_only(text), username, "your Apple ID");
    let text = EMAIL_RE.replace_all(&text, NoExpand("your Apple ID"));
    py_split(&text).collect::<Vec<_>>().join(" ").chars().take(240).collect()
}

/// One line of the wrapper's output as the sign-in log keeps it: without the Apple ID, e-mail
/// addresses, token-like strings and long numbers; a line holding the password or the code is
/// dropped whole (replacing it in place would show where it was).
pub fn scrub_line(line: &str, username: &str, typed: &[&str]) -> String {
    let line = printable_only(&ANSI_RE.replace_all(line, ""));
    let line = line.trim_end_matches(' ');
    if typed.iter().any(|secret| !secret.is_empty() && line.contains(secret)) {
        return LINE_WITHHELD.to_string();
    }
    let line = without_account(line, username, "<apple id>");
    let line = EMAIL_RE.replace_all(&line, NoExpand("<apple id>"));
    let line = TOKEN_RE.replace_all(&line, NoExpand("<left out>"));
    let line = NUMBER_RE.replace_all(&line, NoExpand("<number>"));
    line.chars().take(400).collect()
}

/// What the wrapper reported during a `--pipe` sign-in, by marker.
#[derive(Debug, Default)]
struct Seen {
    started: bool,
    contacting: bool,
    signed_in: bool,
    login_failed: bool,
    dialog: Option<String>,
    apple_message: Option<String>,
    auth_code: Option<String>,
    auth_message: Option<String>,
    response_type: Option<String>,
    tls_problem: Option<String>,
    token_problem: Option<&'static str>,
}

/// JSON lines from a file descriptor, read without blocking past what the wait announced.
struct LineInput<'f> {
    fd: BorrowedFd<'f>,
    buffer: Vec<u8>,
    closed: bool,
    /// A cancel came in the same read as the message [`LineInput::next_message`] returned.
    cancel_followed: bool,
}

impl<'f> LineInput<'f> {
    fn new(fd: BorrowedFd<'f>) -> LineInput<'f> {
        // room for everything feed() ever keeps: the buffer is not moved while it holds a password
        LineInput { fd, buffer: Vec::with_capacity(32768), closed: false, cancel_followed: false }
    }

    /// Read what is available; return the complete messages (invalid lines are dropped).
    fn feed(&mut self) -> Vec<Map<String, Value>> {
        let mut chunk = [0u8; 4096];
        let count = read_some(self.fd, &mut chunk).unwrap_or(0);
        if count == 0 {
            self.closed = true;
            return Vec::new();
        }
        self.buffer.extend_from_slice(&chunk[..count]);
        wipe(&mut chunk);
        if self.buffer.len() > 16384 && !self.buffer.contains(&b'\n') {
            wipe(&mut self.buffer); // not our protocol
            self.buffer.clear();
        }
        let mut messages = Vec::new();
        let mut start = 0;
        while let Some(length) = self.buffer[start..].iter().position(|byte| *byte == b'\n') {
            if let Ok(Value::Object(message)) = serde_json::from_slice(&self.buffer[start..start + length]) {
                messages.push(message);
            }
            start += length + 1;
        }
        wipe(&mut self.buffer[..start]);
        self.buffer.drain(..start);
        for unused in self.buffer.spare_capacity_mut() {
            unused.write(0);
        }
        messages
    }

    /// The next message, `None` when the input closed or `timeout` passed.
    fn next_message(&mut self, helper: &Helper<'_>, timeout: f64) -> Result<Option<Map<String, Value>>, HelperError> {
        let deadline = helper.monotonic() + timeout;
        while !self.closed {
            let remaining = deadline - helper.monotonic();
            if remaining <= 0.0 {
                return Ok(None);
            }
            if helper.wait_readable(&[self.fd], Some(remaining.min(1.0)))?[0] {
                let mut messages = self.feed().into_iter();
                if let Some(first) = messages.next() {
                    self.cancel_followed = messages.any(|message| truthy(message.get("cancel")));
                    return Ok(Some(first));
                }
            }
        }
        Ok(None)
    }
}

// ----------------------------------------------------------------------------- the helper

/// The helper lock, held until it is dropped.
pub struct HelperLock {
    file: File,
}

impl Drop for HelperLock {
    fn drop(&mut self) {
        // Given back explicitly: closing the file alone leaves the lock with a copy of the
        // descriptor that a program being started by another thread may hold for a moment.
        // SAFETY: flock() on a descriptor this lock owns.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// The end of a sign-in attempt, whatever happened in it: the launcher is stopped, the argument
/// file removed, the earlier sign-in's files put back unless the attempt succeeded.
struct Attempt<'h, 'a> {
    helper: &'h Helper<'a>,
    args_path: &'h Path,
    child: Option<Child>,
    master: Option<OwnedFd>,
    succeeded: bool,
}

impl<'h, 'a> Attempt<'h, 'a> {
    fn new(helper: &'h Helper<'a>, args_path: &'h Path) -> Attempt<'h, 'a> {
        Attempt { helper, args_path, child: None, master: None, succeeded: false }
    }

    /// `Popen.poll()`: how the launcher ended, `None` while it runs or when it never started.
    fn exit_status(&mut self) -> Option<i32> {
        self.child.as_mut()?.try_wait().ok()?.map(return_code)
    }
}

impl Drop for Attempt<'_, '_> {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            self.helper.end_launcher(child);
        }
        self.master.take();
        let _ = fs::remove_file(self.args_path);
        if self.succeeded {
            self.helper.drop_kept_account_files();
        } else {
            let _ = self.helper.restore_account_files();
        }
        self.helper.tighten_permissions();
    }
}

/// The commands of the helper over one [`Context`]: the script's functions.
pub struct Helper<'a> {
    pub context: &'a Context,
}

impl<'a> Helper<'a> {
    pub fn new(context: &'a Context) -> Helper<'a> {
        Helper { context }
    }

    fn paths(&self) -> &'a Paths {
        &self.context.paths
    }

    /// One line on standard output (`print()`).
    fn say(&self, text: &str) -> Result<(), HelperError> {
        write_all(self.context.stdout.as_fd(), format!("{text}\n").as_bytes())
            .map_err(|error| HelperError::io("standard output", error))
    }

    /// One line on standard error; nobody listening is not a failure of the command.
    fn warn(&self, text: &str) {
        let _ = write_all(self.context.stderr.as_fd(), format!("{text}\n").as_bytes());
    }

    /// One JSON line for the window. A closed pipe (the window is gone) is the reader's problem.
    fn emit(&self, event: &Value) {
        let _ = write_all(self.context.stdout.as_fd(), format!("{}\n", json_dumps(event, None)).as_bytes());
    }

    fn emit_phase(&self, phase: &str) {
        self.emit(&json!({"event": "phase", "phase": phase}));
    }

    fn monotonic(&self) -> f64 {
        self.context.clock.monotonic().as_secs_f64()
    }

    fn now_ns(&self) -> u128 {
        self.context.clock.now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_nanos())
    }

    fn local_stamp(&self) -> String {
        local_stamp(self.context.clock.now())
    }

    fn check_interrupt(&self) -> Result<(), HelperError> {
        match self.context.interrupt.raised() {
            Some(_) => Err(HelperError::Interrupted),
            None => Ok(()),
        }
    }

    /// Wait until one of `fds` can be read (data or the end of it) or `seconds` have passed;
    /// which ones can. An interruption ends the wait at once and for good.
    fn wait_readable(&self, fds: &[BorrowedFd<'_>], seconds: Option<f64>) -> Result<Vec<bool>, HelperError> {
        let mut polled: Vec<libc::pollfd> =
            fds.iter().map(|fd| libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 }).collect();
        polled.push(libc::pollfd { fd: self.context.interrupt.reader.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        let deadline = seconds.map(|seconds| Instant::now() + Duration::from_secs_f64(seconds.max(0.0)));
        loop {
            let wait = match deadline {
                None => -1,
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    left.as_micros().div_ceil(1000).min(i32::MAX as u128) as libc::c_int
                }
            };
            // SAFETY: polls a live array of the given length.
            if unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, wait) } < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(HelperError::io("waiting for input", error));
            }
            if polled.last().is_some_and(|interrupt| interrupt.revents != 0) {
                return Err(HelperError::Interrupted);
            }
            return Ok(polled[..fds.len()].iter().map(|fd| fd.revents != 0).collect());
        }
    }

    /// `time.sleep()`, ended by an interruption.
    fn sleep(&self, seconds: f64) -> Result<(), HelperError> {
        self.wait_readable(&[], Some(seconds)).map(|_| ())
    }

    /// `Popen.wait(timeout)`: how the process ended, `None` when `deadline` (on the helper's
    /// clock) came first.
    fn wait_child_until(&self, child: &mut Child, deadline: f64) -> Result<Option<ExitStatus>, HelperError> {
        let mut delay = 0.001;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(Some(status)),
                Ok(None) => {}
                // The calling process does not let its children be waited for (SIGCHLD ignored):
                // it has ended, and as in Python its status counts as 0.
                Err(_) => return Ok(Some(ExitStatus::from_raw(0))),
            }
            if self.monotonic() >= deadline {
                return Ok(None);
            }
            self.sleep(delay)?;
            delay = (delay * 2.0).min(0.05);
        }
    }

    /// `Popen.wait()`: how the process ended, however long that takes.
    fn wait_child(&self, child: &mut Child) -> Result<ExitStatus, HelperError> {
        loop {
            if let Some(status) = self.wait_child_until(child, f64::INFINITY)? {
                return Ok(status);
            }
        }
    }

    /// The same wait for clean-up code: nothing cuts it short.
    fn wait_child_quietly(&self, child: &mut Child, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        let mut delay = Duration::from_millis(1);
        loop {
            if !matches!(child.try_wait(), Ok(None)) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_millis(50));
        }
    }

    // ------------------------------------------------------------------------- utilities

    /// True when nothing listens on HOST:PORT (a connect attempt is refused).
    ///
    /// A bind test is wrong here: connections the stopped wrapper closed stay in TIME-WAIT on
    /// this port for 60 s and block a plain or SO_REUSEADDR bind, while the wrapper itself
    /// (cpp-httplib binds with SO_REUSEPORT) rebinds over them without trouble. Only a live
    /// listener accepts a connection.
    pub fn port_free(&self) -> Result<bool, HelperError> {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, self.context.port));
        let free = match TcpStream::connect_timeout(&address, Duration::from_secs(1)) {
            Ok(_) => false,
            // anything else is unexpected (timeout, filtered); treat as busy
            Err(error) => error.kind() == io::ErrorKind::ConnectionRefused,
        };
        self.check_interrupt()?;
        Ok(free)
    }

    /// Owner-only modes below the data directory. Apple's libraries create some files 0664/0644
    /// (e.g. adi.pb); the directory is 0700 anyway, this just removes the inconsistency.
    pub fn tighten_permissions(&self) {
        fn tighten(folder: &Path) {
            let Ok(entries) = fs::read_dir(folder) else {
                return;
            };
            let mut folders = Vec::new();
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_symlink() {
                    continue;
                }
                let mode = if kind.is_dir() { 0o700 } else { 0o600 };
                let _ = fs::set_permissions(entry.path(), Permissions::from_mode(mode));
                if kind.is_dir() {
                    folders.push(entry.path());
                }
            }
            for folder in folders {
                tighten(&folder);
            }
        }
        if self.paths().data.is_dir() {
            tighten(&self.paths().data);
        }
    }

    /// Wait until no listener answers on HOST:PORT (the signalled wrapper may still be shutting
    /// down).
    pub fn wait_port_free(&self, timeout: f64) -> Result<bool, HelperError> {
        let deadline = self.monotonic() + timeout;
        loop {
            if self.port_free()? {
                return Ok(true);
            }
            if self.monotonic() >= deadline {
                return Ok(false);
            }
            self.sleep(0.25)?;
        }
    }

    /// The storefront list of the wrapper's /status payload, or `None` when nothing answers
    /// like the wrapper.
    pub fn http_status(&self, timeout: f64) -> Result<Option<Vec<Value>>, HelperError> {
        let regions = status_regions(&self.context.status_url(), Duration::from_secs_f64(timeout));
        self.check_interrupt()?;
        Ok(regions)
    }

    pub fn read_pidfile(&self) -> Option<Map<String, Value>> {
        util::read_json_object(&self.paths().pidfile).filter(|info| info.contains_key("pid"))
    }

    /// The launcher of the pidfile when it is still the same process (pid and start time
    /// match).
    fn pidfile_process(&self, info: &Map<String, Value>) -> Option<i32> {
        let pid = info.get("pid")?.as_i64().filter(|pid| (2..=i32::MAX as i64).contains(pid))? as i32;
        let system = &self.context.system;
        let start = system.proc_start_time(pid)?;
        if info.get("start_time").and_then(Value::as_u64) != Some(start) {
            return None;
        }
        let cmd = system.proc_cmdline(pid);
        let name = Path::new(cmd.first()?).file_name()?;
        (Some(name) == self.paths().launcher.file_name()).then_some(pid)
    }

    /// True when the recorded launcher is still the same process (pid and start time match).
    pub fn pidfile_process_alive(&self, info: &Map<String, Value>) -> bool {
        self.pidfile_process(info).is_some()
    }

    /// What a launcher of this build or the wrapper it runs in this runtime can be running as.
    fn wrapper_programs(&self) -> [PathBuf; 4] {
        let launcher = self.paths().launcher.clone();
        let wrapper = self.paths().rootfs.join("system/bin/lite");
        [realpath(&launcher), launcher, realpath(&wrapper), wrapper]
    }

    fn is_one_of(&self, pid: i32, programs: &[PathBuf; 4]) -> bool {
        let system = &self.context.system;
        // started by the full path, as earlier versions of this helper did
        if system.proc_cmdline(pid).first().is_some_and(|program| Path::new(program) == self.paths().launcher) {
            return true;
        }
        let Ok(program) = system.proc_program(pid) else {
            return false;
        };
        // "(deleted)": the file was replaced by a newer build while it runs
        let program = Path::new(program.strip_suffix(" (deleted)").unwrap_or(&program));
        programs.iter().any(|candidate| candidate == program)
    }

    /// True when `pid` is a launcher of this build or the wrapper it runs in this runtime,
    /// however it was started.
    pub fn is_wrapper_process(&self, pid: i32) -> bool {
        self.is_one_of(pid, &self.wrapper_programs())
    }

    /// Resolvers for the wrapper: LOSSLESS_DNS, else the nameserver lines of /etc/resolv.conf.
    pub fn host_resolvers(&self) -> Vec<String> {
        let raw = self.context.env.get("LOSSLESS_DNS").map_or("", |value| py_strip(value));
        let mut candidates: Vec<String> = Vec::new();
        if !raw.is_empty() {
            candidates.extend(py_split(&raw.replace(',', " ")).map(String::from));
        } else if let Ok(text) = fs::read(&self.paths().resolv_conf) {
            for line in py_splitlines(&String::from_utf8_lossy(&text)) {
                let parts: Vec<&str> = py_split(line).collect();
                if parts.len() >= 2 && parts[0] == "nameserver" {
                    candidates.push(parts[1].split('%').next().unwrap_or_default().to_string());
                }
            }
        }
        let mut valid = Vec::new();
        for item in candidates {
            match ip_address(&item) {
                Some(address) => valid.push(address),
                None => self.warn(&format!("ignoring invalid resolver address {}", py_repr(&item))),
            }
        }
        valid.truncate(4);
        valid
    }

    /// The environment of the service.
    pub fn service_env(&self) -> Vec<(String, String)> {
        let mut env = base_env();
        let resolvers = self.host_resolvers();
        if resolvers.is_empty() {
            self.warn("warning: no host resolver found; the wrapper falls back to its upstream public resolvers");
        } else {
            env.push(("LITE_DNS_SERVERS".to_string(), resolvers.join(",")));
        }
        env
    }

    pub fn require_build(&self) -> Result<(), HelperError> {
        if !self.paths().launcher.is_file() || !self.paths().built_lite.is_file() {
            let build = self.paths().here.join("build.sh");
            return Err(HelperError::refused(format!("wrapper not built; run {} first", build.display())));
        }
        Ok(())
    }

    pub fn require_runtime(&self) -> Result<(), HelperError> {
        let rootfs = &self.paths().rootfs;
        if !rootfs.join("system/bin/lite").is_file() || !rootfs.join("system/etc/workspace-ca-bundle.pem").is_file() {
            return Err(HelperError::refused(format!(
                "runtime not staged under {}; run {} first",
                self.paths().runtime.display(),
                self.paths().here.join("build.sh").display()
            )));
        }
        Ok(())
    }

    /// A sign-in runs only through a wrapper built with patches/0004 (one answer per credential
    /// request).
    ///
    /// An older build repeats a refused password to Apple by itself, and every refusal counts
    /// toward Apple's account lockout; the checks of this helper come too late to stop that.
    pub fn require_one_answer_wrapper(&self) -> Result<(), HelperError> {
        let patched = fs::read(self.paths().rootfs.join("system/bin/lite"))
            .is_ok_and(|program| count_occurrences(&program, ONE_ANSWER_MARKER) > 0);
        if !patched {
            return Err(HelperError::refused(
                "The lossless route on this computer was built before its sign-in answered Apple only once per \
                 attempt (a mistyped password could be sent twice). Update it first: quit am-ui, run \
                 tools/lossless/stop.sh, then ./setup.sh. A sign-in you already have keeps working.",
            ));
        }
        Ok(())
    }

    /// `path` as one types it in the am-ui folder when it lies inside it.
    pub fn shown_path(&self, path: &Path) -> String {
        match path.strip_prefix(&self.paths().root) {
            Ok(inside) if inside.as_os_str().is_empty() => ".".to_string(),
            Ok(inside) => inside.display().to_string(),
            Err(_) => path.display().to_string(),
        }
    }

    /// The settings of this system that keep ordinary programs from using user namespaces.
    pub fn userns_settings(&self) -> Vec<String> {
        // (setting, whether it is the value 0 that forbids them)
        let settings = [
            ("kernel/apparmor_restrict_unprivileged_userns", false),
            ("kernel/unprivileged_userns_clone", true),
            ("user/max_user_namespaces", true),
        ];
        let mut found = Vec::new();
        for (name, zero_forbids) in settings {
            if let Some(value) = self.context.system.sysctl(name).filter(|value| (*value == 0) == zero_forbids) {
                found.push(format!("{}={value}", name.replace('/', ".")));
            }
        }
        found
    }

    /// Why the wrapper did not start, from what the launcher and the loader printed before it
    /// (system messages only; nothing of an account is in them).
    pub fn explain_start_failure<S: AsRef<str>>(&self, lines: &[S]) -> String {
        let lines: Vec<String> = lines
            .iter()
            .map(AsRef::as_ref)
            .filter(|line| !py_strip(line).is_empty())
            .map(|line| printable(line, 200))
            .collect();
        for line in &lines {
            let low = line.to_lowercase();
            if NAMESPACE_STEPS.iter().any(|step| low.starts_with(step)) && line.contains(':') {
                let settings = self.userns_settings();
                let setting = match settings.is_empty() {
                    true => String::new(),
                    false => format!("; setting in the way: {}", settings.join(", ")),
                };
                return format!("this system did not let it set up the private namespace it runs in ({line}){setting}");
            }
            if FOLDER_STEPS.iter().any(|step| low.starts_with(step)) && line.contains(':') {
                let runtime = self.shown_path(&self.paths().runtime);
                return format!("it could not prepare its folder under {runtime} ({line})");
            }
            if low.starts_with("execve") {
                let noexec = match low.contains("permission") {
                    true => "; the am-ui folder may be on a file system mounted noexec",
                    false => "",
                };
                return format!("this system refused to run its program ({line}){noexec}");
            }
            if low.contains("cannot link executable") || low.starts_with("linker:") {
                // a build made before build.sh named the rootfs's libraries to CMake
                if let Some(foreign) = FOREIGN_LIBRARY_RE.captures(line) {
                    return format!(
                        "it was built against this system's {} instead of the library that comes with it, so it \
                         cannot be loaded; `./setup.sh` builds it again ({line})",
                        &foreign[1]
                    );
                }
                return format!("its program could not be loaded ({line})");
            }
        }
        match lines.last() {
            Some(last) => format!("it stopped with: {last}"),
            None => "it ended without printing anything (the kernel log, `journalctl -k -n 20`, may say what ended it)"
                .to_string(),
        }
    }

    /// Run the wrapper just far enough to print its usage text: the launcher's user namespace,
    /// its mounts and chroot, the Android loader and every library the wrapper links, and
    /// nothing else (no account file is read, nothing goes to the network). Returns (starts,
    /// reason when it does not).
    pub fn launcher_selftest(&self) -> Result<(bool, String), HelperError> {
        let pipe = |error| HelperError::io("a pipe for the start check", error);
        let (mut reader, writer) = io::pipe().map_err(pipe)?;
        let launch = Launch {
            program: self.paths().launcher.clone(),
            argv: launcher_command(&["--help"]),
            cwd: self.paths().runtime.clone(),
            env: base_env(),
            stdin: Stdio::null(),
            stdout: Stdio::from(writer.try_clone().map_err(pipe)?),
            stderr: Stdio::from(writer),
            new_session: true,
        };
        let mut child = match self.context.system.spawn(launch) {
            Ok(child) => child,
            Err(error) => return Ok((false, format!("its launcher could not be run ({})", strerror(&error)))),
        };
        let deadline = self.monotonic() + SELFTEST_TIMEOUT_S;
        let mut output = Vec::new();
        let mut chunk = [0u8; 4096];
        let answered = loop {
            let remaining = deadline - self.monotonic();
            if remaining <= 0.0 {
                break false;
            }
            match self.wait_readable(&[reader.as_fd()], Some(remaining.min(1.0))) {
                Ok(ready) if !ready[0] => {}
                Ok(_) => match reader.read(&mut chunk) {
                    Ok(0) => break true,
                    // the usage text and the loader's complaints are short: more is not kept
                    Ok(count) if output.len() < SELFTEST_MAX_BYTES => output.extend_from_slice(&chunk[..count]),
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break true,
                },
                Err(error) => {
                    self.end_launcher(&mut child);
                    return Err(error);
                }
            }
        };
        let ended = match answered {
            true => match self.wait_child_until(&mut child, deadline) {
                Ok(status) => status.is_some(),
                Err(error) => {
                    self.end_launcher(&mut child);
                    return Err(error);
                }
            },
            false => false,
        };
        if !ended {
            self.end_launcher(&mut child);
            return Ok((false, format!("it did not answer within {SELFTEST_TIMEOUT_S:.0} seconds")));
        }
        let output = String::from_utf8_lossy(&output);
        let lines = py_splitlines(&output);
        if lines.iter().any(|line| line.contains("usage: lite")) {
            return Ok((true, String::new()));
        }
        Ok((false, self.explain_start_failure(&lines)))
    }

    pub fn require_startable(&self) -> Result<(), HelperError> {
        let (starts, reason) = self.launcher_selftest()?;
        if !starts {
            return Err(HelperError::refused(format!(
                "Apple's component for the sign-in and for lossless cannot start on this computer: {reason}"
            )));
        }
        Ok(())
    }

    pub fn cmd_selftest(&self) -> Result<i32, HelperError> {
        self.require_build()?;
        self.require_runtime()?;
        self.require_startable()?;
        self.say("ok: the wrapper starts on this system")?;
        Ok(0)
    }

    /// One helper command at a time in this workspace (`flock`).
    pub fn helper_lock(&self) -> Result<HelperLock, HelperError> {
        let paths = self.paths();
        util::create_private_dirs(&paths.run).map_err(|error| HelperError::io(paths.run.display(), error))?;
        let lock_error = |error| HelperError::io(paths.lockfile.display(), error);
        let file =
            OpenOptions::new().append(true).create(true).mode(0o600).open(&paths.lockfile).map_err(lock_error)?;
        loop {
            // SAFETY: flock() on a descriptor that is open for the duration of the call.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(HelperLock { file });
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EWOULDBLOCK) => {
                    return Err(HelperError::refused("another lossless helper command is running in this workspace"));
                }
                Some(libc::EINTR) => {}
                _ => return Err(lock_error(error)),
            }
        }
    }

    /// Presence/size-only view of the account files; contents are never read.
    pub fn signed_in_state(&self) -> Value {
        let data = &self.paths().data;
        let present = |name: &str| fs::metadata(data.join(name)).is_ok_and(|file| file.is_file() && file.len() > 0);
        let database = data.join("mpl_db");
        json!({
            "music_token_file": present("MUSIC_TOKEN"),
            "storefront_file": present("STOREFRONT_ID"),
            "dev_token_file": present("DEV_TOKEN"),
            "account_database": fs::read_dir(&database).is_ok_and(|mut entries| entries.next().is_some()),
            "login_in_progress": data.join(".login-args").exists(),
        })
    }

    // ------------------------------------------------------------------------- commands

    pub fn cmd_paths(&self, sh: bool) -> Result<i32, HelperError> {
        let paths = self.paths();
        let shown = |path: &Path| path.display().to_string();
        let values = [
            ("AMUI_LOSSLESS_ROOT", shown(&paths.root)),
            ("AMUI_LOSSLESS_BUILD", shown(&paths.build)),
            ("AMUI_LOSSLESS_RUNTIME", shown(&paths.runtime)),
            ("AMUI_LOSSLESS_DATA", shown(&paths.data)),
            ("AMUI_LOSSLESS_LAUNCHER", shown(&paths.launcher)),
            ("AMUI_LOSSLESS_PORT", self.context.port.to_string()),
            ("AMUI_LOSSLESS_STATUS_URL", self.context.status_url()),
            ("AMUI_LOSSLESS_SERVICE_LOG", shown(&paths.data.join("service.log"))),
        ];
        if sh {
            // in one piece, as Python hands a pipe everything it printed when it ends
            let lines: Vec<String> = values
                .into_iter()
                .map(|(key, value)| format!("export {key}={}", json_dumps(&Value::String(value), None)))
                .collect();
            self.say(&lines.join("\n"))?;
        } else {
            let values: Map<String, Value> =
                values.into_iter().map(|(key, value)| (key.to_string(), Value::String(value))).collect();
            self.say(&json_dumps(&Value::Object(values), Some(2)))?;
        }
        Ok(0)
    }

    /// SHA-256 over the files of tools/lossless that decide what the build produces. build.sh
    /// stores it in the build manifest; a different value later means the build is older than
    /// these files.
    pub fn source_fingerprint(&self) -> Result<String, HelperError> {
        let here = &self.paths().here;
        let mut files =
            vec![here.join("build.sh"), self.paths().pin_file.clone(), here.join("go/amui-fairplay-rip/main.go")];
        let mut patches: Vec<PathBuf> = match fs::read_dir(here.join("patches")) {
            Ok(entries) => entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.file_name().is_some_and(|name| name.as_bytes().ends_with(b".patch")))
                .collect(),
            Err(_) => Vec::new(),
        };
        patches.sort();
        files.extend(patches);
        let mut digest = Sha256::new();
        for path in &files {
            digest.update(path.strip_prefix(here).unwrap_or(path).as_os_str().as_bytes());
            digest.update([0u8]);
            let content = fs::read(path).map_err(|error| HelperError::io(path.display(), error))?;
            digest.update(Sha256::digest(&content));
        }
        Ok(hex(&digest.finalize()))
    }

    pub fn cmd_fingerprint(&self) -> Result<i32, HelperError> {
        self.say(&self.source_fingerprint()?)?;
        Ok(0)
    }

    /// absent: nothing usable is built. stale: built from other inputs than today's
    /// tools/lossless. unstaged: built from today's inputs, but the runtime does not hold that
    /// build. current: both.
    pub fn build_state(&self) -> Result<&'static str, HelperError> {
        let paths = self.paths();
        let outputs = [
            &paths.launcher,
            &paths.built_lite,
            &paths.build.join("bin/amdl"),
            &paths.build.join("bin/amui-fairplay-rip"),
        ];
        if !outputs.iter().all(|path| path.is_file()) {
            return Ok("absent");
        }
        let recorded = util::read_json_object(&paths.build.join("manifest.json"))
            .and_then(|manifest| manifest.get("source_fingerprint").and_then(Value::as_str).map(String::from));
        if recorded != Some(self.source_fingerprint()?) {
            return Ok("stale");
        }
        let staged = paths.rootfs.join("system/bin");
        let same = ["lite", "linker64"].iter().all(|name| {
            match (sha256_file(&staged.join(name)), sha256_file(&paths.built_system.join("bin").join(name))) {
                (Ok(staged), Ok(built)) => staged == built,
                _ => false,
            }
        });
        if !same
            || !paths.rootfs.join("system/etc/workspace-ca-bundle.pem").is_file()
            || !paths.runtime.join("amdl/config.yaml").is_file()
        {
            return Ok("unstaged");
        }
        Ok("current")
    }

    /// What makes `stage` refuse: something answers on the port, or the launcher of the pidfile
    /// lives.
    pub fn service_running(&self) -> Result<bool, HelperError> {
        let info = self.read_pidfile();
        Ok(self.http_status(3.0)?.is_some() || info.is_some_and(|info| self.pidfile_process_alive(&info)))
    }

    pub fn cmd_build_state(&self) -> Result<i32, HelperError> {
        let state = self.build_state()?;
        self.say(&format!("{state} {}", if self.service_running()? { "running" } else { "stopped" }))?;
        Ok(0)
    }

    /// The pinned libraries: (file name, SHA-256), in the order of the pin file.
    pub fn pinned_hashes(&self) -> Result<Vec<(String, String)>, HelperError> {
        let pin_file = &self.paths().pin_file;
        let text = fs::read_to_string(pin_file).map_err(|error| HelperError::io(pin_file.display(), error))?;
        let mut pins: Vec<(String, String)> = Vec::new();
        for line in py_splitlines(&text) {
            let parts: Vec<&str> = py_split(line).collect();
            if parts.len() == 2 && parts[0].chars().count() == 64 && !line.starts_with('#') {
                match pins.iter_mut().find(|(name, _)| name == parts[1]) {
                    Some(pin) => pin.1 = parts[0].to_string(),
                    None => pins.push((parts[1].to_string(), parts[0].to_string())),
                }
            }
        }
        if pins.is_empty() {
            return Err(HelperError::refused(format!("no pinned hashes in {}", pin_file.display())));
        }
        Ok(pins)
    }

    pub fn cmd_stage(&self) -> Result<i32, HelperError> {
        self.require_build()?;
        let paths = self.paths();
        let at = |path: &Path| {
            let shown = path.display().to_string();
            move |error: io::Error| HelperError::io(shown, error)
        };
        let _lock = self.helper_lock()?;
        if self.service_running()? {
            return Err(HelperError::refused("the service is running; stop it before restaging the runtime"));
        }
        for directory in [&paths.var, &paths.runtime, &paths.run, &paths.logs, &paths.data] {
            util::create_private_dirs(directory).map_err(at(directory))?;
            fs::set_permissions(directory, Permissions::from_mode(0o700)).map_err(at(directory))?;
        }
        let system = paths.rootfs.join("system");
        for directory in [&paths.rootfs, &system, &system.join("bin"), &system.join("lib64"), &system.join("etc")] {
            make_dir(directory).map_err(at(directory))?;
        }

        let pins = self.pinned_hashes()?;
        let staged_at = self.local_stamp();
        let lib_src = paths.built_system.join("lib64");
        let mut libs: Vec<PathBuf> = fs::read_dir(&lib_src)
            .map_err(at(&lib_src))?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "so"))
            .collect();
        libs.sort();
        for (name, want) in &pins {
            let path = lib_src.join(name);
            if !path.is_file() {
                return Err(HelperError::refused(format!("pinned library missing from the build tree: {name}")));
            }
            let got = sha256_file(&path).map_err(at(&path))?;
            if &got != want {
                return Err(HelperError::refused(format!(
                    "{name} does not match its pinned SHA-256 ({got}); refusing to stage"
                )));
            }
        }
        let mut copied = 0;
        let mut libraries = Map::new();
        for lib in &libs {
            let name = lib.file_name().unwrap_or_default().to_string_lossy().into_owned();
            copied += copy_if_changed(lib, &system.join("lib64").join(&name), 0o444).map_err(at(lib))? as u32;
            let pinned = pins.iter().any(|(pin, _)| *pin == name);
            libraries.insert(name, json!({"sha256": sha256_file(lib).map_err(at(lib))?, "pinned": pinned}));
        }
        let mut binaries = Map::new();
        for name in ["linker64", "lite"] {
            let src = paths.built_system.join("bin").join(name);
            if !src.is_file() {
                return Err(HelperError::refused(format!("missing build output {}", src.display())));
            }
            copied += copy_if_changed(&src, &system.join("bin").join(name), 0o755).map_err(at(&src))? as u32;
            binaries.insert(name.to_string(), Value::String(sha256_file(&src).map_err(at(&src))?));
        }
        let launcher = sha256_file(&paths.launcher).map_err(at(&paths.launcher))?;
        binaries.insert(LAUNCHER_ARGV0.to_string(), Value::String(launcher));

        let ca_src = match self.context.env.get("LOSSLESS_CA_BUNDLE").filter(|named| !named.is_empty()) {
            Some(named) => named.clone(),
            None => match paths.ca_candidates.iter().find(|candidate| candidate.is_file()) {
                Some(candidate) => candidate.display().to_string(),
                None => {
                    return Err(HelperError::refused("no host CA bundle found; set LOSSLESS_CA_BUNDLE to a PEM file"));
                }
            },
        };
        let ca_path = Path::new(&ca_src);
        let ca_text = fs::read(ca_path).map_err(at(ca_path))?;
        let certificates = count_occurrences(&ca_text, b"BEGIN CERTIFICATE");
        if certificates < 10 {
            return Err(HelperError::refused(format!("{ca_src} does not look like a CA bundle")));
        }
        let staged_ca = system.join("etc/workspace-ca-bundle.pem");
        copied += copy_if_changed(ca_path, &staged_ca, 0o444).map_err(at(ca_path))? as u32;

        let mut manifest = Map::new();
        manifest.insert("staged_at".to_string(), Value::String(staged_at));
        manifest.insert("libraries".to_string(), Value::Object(libraries));
        manifest.insert("binaries".to_string(), Value::Object(binaries));
        let ca_bundle =
            json!({"source": ca_src, "sha256": hex(&Sha256::digest(&ca_text)), "certificates": certificates});
        manifest.insert("ca_bundle".to_string(), ca_bundle);
        let build_manifest = paths.build.join("manifest.json");
        if build_manifest.is_file() {
            let text = fs::read_to_string(&build_manifest).map_err(at(&build_manifest))?;
            let build: Value = serde_json::from_str(&text).map_err(|_| {
                HelperError::failed("JSONDecodeError", format!("{} is not JSON", build_manifest.display()))
            })?;
            manifest.insert("build".to_string(), build);
        }
        let runtime_manifest = paths.runtime.join("manifest.json");
        let text = format!("{}\n", json_dumps(&Value::Object(manifest), Some(2)));
        create_file(&runtime_manifest)
            .and_then(|mut file| file.write_all(text.as_bytes()))
            .map_err(at(&runtime_manifest))?;
        self.say(&json_dumps(
            &json!({
                "runtime": paths.runtime.display().to_string(),
                "files_copied": copied,
                "libraries": libs.len(),
                "pinned_verified": pins.len(),
                "ca_bundle": ca_src,
                "account_data_dir": paths.data.display().to_string(),
                "account_state": self.signed_in_state(),
            }),
            Some(2),
        ))?;
        Ok(0)
    }

    /// Start the launcher: in the foreground on the helper's own standard streams (the terminal
    /// sign-in), or detached in its own session with its output in `log_handle`.
    pub fn spawn_launcher(
        &self,
        extra_args: &[&str],
        env: &[(String, String)],
        foreground: bool,
        log_handle: Option<&File>,
    ) -> io::Result<Child> {
        let context = self.context;
        let (stdin, stdout, stderr) = if foreground {
            (
                Stdio::from(context.stdin.try_clone()?),
                Stdio::from(context.stdout.try_clone()?),
                Stdio::from(context.stderr.try_clone()?),
            )
        } else {
            let output: OwnedFd = match log_handle {
                Some(log) => log.try_clone()?.into(),
                None => context.stdout.try_clone()?,
            };
            (Stdio::null(), Stdio::from(output.try_clone()?), Stdio::from(output))
        };
        context.system.spawn(Launch {
            program: self.paths().launcher.clone(),
            argv: launcher_command(extra_args),
            cwd: self.paths().runtime.clone(),
            env: env.to_vec(),
            stdin,
            stdout,
            stderr,
            new_session: !foreground,
        })
    }

    pub fn cmd_start(&self) -> Result<i32, HelperError> {
        self.require_build()?;
        self.require_runtime()?;
        let paths = self.paths();
        let port = self.context.port;
        let at = |path: &Path| {
            let shown = path.display().to_string();
            move |error: io::Error| HelperError::io(shown, error)
        };
        let _lock = self.helper_lock()?;
        let info = self.read_pidfile();
        if let Some(info) = &info
            && self.pidfile_process_alive(info)
            && self.http_status(3.0)?.is_some()
        {
            let report = json!({"running": true, "already": true, "pid": info["pid"], "port": port});
            self.say(&json_dumps(&report, Some(2)))?;
            return Ok(0);
        }
        let login_args = paths.data.join(".login-args");
        if login_args.exists() {
            return Err(HelperError::refused(format!(
                "a login argument file exists in the runtime (login in progress or interrupted); finish or clean \
                 it up first: {}",
                login_args.display()
            )));
        }
        let launchers = self.other_launchers();
        if launchers.is_empty() {
            // a sign-in that was killed may have left the account files moved aside
            self.restore_account_files().map_err(at(&paths.data))?;
        }
        if !self.port_free()? {
            if self.http_status(3.0)?.is_some() {
                return Err(HelperError::refused(format!(
                    "another wrapper already answers on {HOST}:{port} and was not started by this helper; stop it \
                     yourself or choose LOSSLESS_PORT"
                )));
            }
            if !self.wait_port_free(15.0)? {
                return Err(HelperError::refused(format!(
                    "{HOST}:{port} is in use by another program; choose LOSSLESS_PORT"
                )));
            }
        }
        if !launchers.is_empty() {
            // a sign-in in progress uses the same account database
            return Err(HelperError::refused(
                "a sign-in of this wrapper is still running; start the service once it has ended",
            ));
        }
        if info.is_some() {
            remove_file_if_exists(&paths.pidfile).map_err(at(&paths.pidfile))?;
        }
        let env = self.service_env();
        let launcher_log = paths.logs.join("launcher.log");
        let service_log = paths.data.join("service.log");
        let mut log =
            OpenOptions::new().append(true).create(true).mode(0o600).open(&launcher_log).map_err(at(&launcher_log))?;
        log.set_permissions(Permissions::from_mode(0o600)).map_err(at(&launcher_log))?;
        log.write_all(format!("\n--- start {} port {port}\n", self.local_stamp()).as_bytes())
            .map_err(at(&launcher_log))?;
        let arguments = ["--host", HOST, "--port", &port.to_string(), "--log-file", "/data/service.log"];
        let mut service =
            Service(Some(self.spawn_launcher(&arguments, &env, false, Some(&log)).map_err(at(&paths.launcher))?));
        drop(log);
        let child = service.0.as_mut().expect("the launcher that was just started");
        let pid = child.id();
        let record = json!({
            "pid": pid,
            "start_time": self.context.system.proc_start_time(pid as i32),
            "port": port,
            "launcher": paths.launcher.display().to_string(),
            "started": self.local_stamp(),
        });
        create_file(&paths.pidfile)
            .and_then(|mut file| file.write_all(format!("{}\n", json_dumps(&record, None)).as_bytes()))
            .and_then(|()| fs::set_permissions(&paths.pidfile, Permissions::from_mode(0o600)))
            .map_err(at(&paths.pidfile))?;
        let deadline = self.monotonic() + 60.0;
        let mut status = None;
        while self.monotonic() < deadline {
            if !matches!(child.try_wait(), Ok(None)) {
                break;
            }
            status = self.http_status(1.0)?;
            if status.is_some() {
                break;
            }
            self.sleep(0.5)?;
        }
        let Some(regions) = status else {
            remove_file_if_exists(&paths.pidfile).map_err(at(&paths.pidfile))?;
            if matches!(child.try_wait(), Ok(None)) {
                signal_group(pid as i32, libc::SIGTERM);
                if !self.wait_child_quietly(child, Duration::from_secs(5)) {
                    signal_group(pid as i32, libc::SIGKILL);
                }
            }
            let (starts, reason) = self.launcher_selftest()?;
            if !starts {
                return Err(HelperError::refused(format!("the wrapper cannot start on this computer: {reason}")));
            }
            return Err(HelperError::refused(format!(
                "the service did not answer on {}; see {} and {}",
                self.context.status_url(),
                launcher_log.display(),
                service_log.display()
            )));
        };
        self.tighten_permissions();
        let resolvers = env.iter().find(|(name, _)| name == "LITE_DNS_SERVERS").map(|(_, value)| value.as_str());
        self.say(&json_dumps(
            &json!({
                "running": true,
                "pid": pid,
                "port": port,
                "status_url": self.context.status_url(),
                "storefronts": regions,
                "signed_in": self.signed_in_state(),
                "resolvers": resolvers.unwrap_or("(upstream defaults)"),
                "service_log": service_log.display().to_string(),
            }),
            Some(2),
        ))?;
        Ok(0)
    }

    /// Stop the service this helper started (never another process). Call with helper_lock()
    /// held. Returns (report, exit status): 0 stopped or not running, 1 processes remain, 2 a
    /// wrapper this helper did not start answers on the port.
    pub fn stop_service(&self) -> Result<(Value, i32), HelperError> {
        let paths = self.paths();
        let pidfile = |error| HelperError::io(paths.pidfile.display(), error);
        let Some(info) = self.read_pidfile() else {
            if self.http_status(3.0)?.is_some() {
                let note = "not started by this helper; left alone";
                return Ok((json!({"running": true, "stopped": false, "note": note}), 2));
            }
            return Ok((json!({"running": false, "stopped": false, "note": "not running"}), 0));
        };
        let Some(pid) = self.pidfile_process(&info) else {
            remove_file_if_exists(&paths.pidfile).map_err(pidfile)?;
            return Ok((json!({"running": false, "stopped": false, "note": "stale pidfile removed"}), 0));
        };
        let system = &self.context.system;
        let children = system.descendants(pid);
        // each with its start time: a number that has passed to another process is left alone
        let targets: Vec<(i32, Option<u64>)> = std::iter::once(pid)
            .chain(children.iter().copied())
            .map(|target| (target, system.proc_start_time(target)))
            .collect();
        let lives =
            |(target, started): &(i32, Option<u64>)| started.is_some() && system.proc_start_time(*target) == *started;
        signal_group(pid, libc::SIGTERM);
        let deadline = self.monotonic() + 15.0;
        while self.monotonic() < deadline && targets.iter().any(lives) {
            self.sleep(0.2)?;
        }
        let mut killed = Vec::new();
        for target in &targets {
            if lives(target) && signal_process(target.0, libc::SIGKILL) {
                killed.push(target.0);
            }
        }
        self.sleep(0.3)?;
        let remaining: Vec<i32> = targets.iter().filter(|target| lives(target)).map(|target| target.0).collect();
        remove_file_if_exists(&paths.pidfile).map_err(pidfile)?;
        self.tighten_permissions();
        let port_is_free = if remaining.is_empty() { self.wait_port_free(15.0)? } else { self.port_free()? };
        let status = if remaining.is_empty() { 0 } else { 1 };
        let report = json!({
            "running": false,
            "stopped": true,
            "pid": pid,
            "children": children,
            "force_killed": killed,
            "remaining": remaining,
            "port_free": port_is_free,
        });
        Ok((report, status))
    }

    pub fn cmd_stop(&self) -> Result<i32, HelperError> {
        let (report, code) = {
            let _lock = self.helper_lock()?;
            self.stop_service()?
        };
        if code == 2 {
            self.warn(&format!(
                "a wrapper answers on {HOST}:{} but was not started by this helper; leaving it alone",
                self.context.port
            ));
            return Ok(2);
        }
        self.say(&json_dumps(&report, Some(2)))?;
        Ok(code)
    }

    pub fn cmd_status(&self) -> Result<i32, HelperError> {
        let paths = self.paths();
        let info = self.read_pidfile();
        let alive = info.as_ref().is_some_and(|info| self.pidfile_process_alive(info));
        let status = self.http_status(3.0)?;
        let mut report = json!({
            "running": status.is_some(),
            "started_by_helper": alive,
            "pid": info.as_ref().filter(|_| alive).map(|info| info["pid"].clone()),
            "port": self.context.port,
            "status_url": self.context.status_url(),
            "storefronts": status,
            "signed_in": status.as_ref().is_some_and(|regions| !regions.is_empty()),
            "account_files": paths.data.is_dir().then(|| self.signed_in_state()),
            "runtime": paths.runtime.display().to_string(),
            "runtime_staged": paths.rootfs.join("system/bin/lite").is_file(),
            "built": paths.launcher.is_file() && paths.built_lite.is_file(),
            "service_log": paths.data.join("service.log").display().to_string(),
        });
        let note = match (status.is_some(), alive) {
            (false, true) => Some("launcher process alive but /status does not answer"),
            (true, false) => Some("answers on the port but was not started by this helper"),
            _ => None,
        };
        if let (Some(note), Some(report)) = (note, report.as_object_mut()) {
            report.insert("note".to_string(), Value::String(note.to_string()));
        }
        self.say(&json_dumps(&report, Some(2)))?;
        Ok(if status.is_some() { 0 } else { 1 })
    }

    /// PIDs of this build's launchers and wrappers that do not belong to the recorded service
    /// (an earlier sign-in that is still running, or a wrapper whose launcher was killed).
    pub fn other_launchers(&self) -> Vec<i32> {
        let system = &self.context.system;
        let mut ours = vec![std::process::id() as i32];
        if let Some(service) = self.read_pidfile().and_then(|info| self.pidfile_process(&info)) {
            ours.push(service);
            ours.extend(system.descendants(service));
        }
        let programs = self.wrapper_programs();
        let mut found = Vec::new();
        for pid in system.pids() {
            if ours.contains(&pid) || !self.is_one_of(pid, &programs) {
                continue;
            }
            // a start check (selftest): it opens nothing
            if system.proc_cmdline(pid).last().is_some_and(|last| last == "--help") {
                continue;
            }
            found.push(pid);
        }
        found
    }

    /// Checks before a sign-in, with helper_lock() held: nothing else may use the account
    /// database. Returns the path of the argument file to write.
    pub fn login_preflight(&self, stop: bool, clear_stale_args: bool) -> Result<PathBuf, HelperError> {
        let running = |helper: &Helper<'_>| -> Result<bool, HelperError> {
            let alive = helper.read_pidfile().is_some_and(|info| helper.pidfile_process_alive(&info));
            Ok(alive || helper.http_status(3.0)?.is_some())
        };
        if running(self)? {
            if stop {
                self.stop_service()?;
            }
            if running(self)? {
                return Err(HelperError::refused(
                    "the wrapper service is running; stop it first (tools/lossless/stop.sh): the login process uses \
                     the same account database",
                ));
            }
        }
        if !self.port_free()? {
            return Err(HelperError::refused(format!(
                "{HOST}:{} is in use; stop whatever listens there before logging in",
                self.context.port
            )));
        }
        if !self.other_launchers().is_empty() {
            return Err(HelperError::refused("another sign-in of this wrapper is still running; wait for it to end"));
        }
        let args_path = self.paths().data.join(".login-args");
        if args_path.symlink_metadata().is_ok() {
            if !clear_stale_args {
                return Err(HelperError::refused(format!(
                    "{} exists: another login is in progress or was interrupted. If no login is running, remove \
                     that file and retry.",
                    args_path.display()
                )));
            }
            // left by an interrupted sign-in; no sign-in runs (checked above)
            fs::remove_file(&args_path).map_err(|error| HelperError::io(args_path.display(), error))?;
        }
        Ok(args_path)
    }

    /// Move the files the wrapper deletes at the start of a sign-in aside (rename; contents
    /// unread). A set kept by an interrupted earlier attempt wins over nothing, never over
    /// existing files.
    pub fn keep_account_files(&self) -> io::Result<()> {
        for name in KEPT_FILES {
            let current = self.paths().data.join(name);
            if current.symlink_metadata().is_ok_and(|file| file.is_file()) {
                fs::rename(&current, self.paths().data.join(format!("{name}{KEEP_SUFFIX}")))?;
            }
        }
        Ok(())
    }

    /// After a failed attempt: put the earlier sign-in's files back where the attempt left none.
    pub fn restore_account_files(&self) -> io::Result<()> {
        let mut result = Ok(());
        for name in KEPT_FILES {
            let current = self.paths().data.join(name);
            let kept = self.paths().data.join(format!("{name}{KEEP_SUFFIX}"));
            if !kept.symlink_metadata().is_ok_and(|file| file.is_file()) {
                continue;
            }
            // one file that cannot be moved does not keep the other from coming back
            let moved = if current.exists() { fs::remove_file(&kept) } else { fs::rename(&kept, &current) };
            result = result.and(moved);
        }
        result
    }

    pub fn drop_kept_account_files(&self) {
        for name in KEPT_FILES {
            let _ = fs::remove_file(self.paths().data.join(format!("{name}{KEEP_SUFFIX}")));
        }
    }

    /// True when the sign-in wrote every account file after `started_ns` (sizes and times only).
    pub fn fresh_account_files(&self, started_ns: u128) -> bool {
        LOGIN_FILES.iter().all(|name| {
            fs::metadata(self.paths().data.join(name)).is_ok_and(|file| {
                let modified_ns = file.mtime() as i128 * 1_000_000_000 + file.mtime_nsec() as i128;
                file.is_file() && file.len() > 0 && modified_ns >= started_ns as i128
            })
        })
    }

    /// The argument file the wrapper reads the credentials from: new, of the owner alone, never
    /// through a link.
    fn write_login_args(&self, args_path: &Path, username: &str, password: &Secret) -> io::Result<()> {
        let mut text = String::with_capacity(username.len() + password.expose().len() + 32);
        text.push_str("--login\n");
        text.push_str(username);
        text.push(':');
        text.push_str(password.expose());
        text.push_str("\n--base-dir\n/data\n");
        let text = Secret::new(text);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(args_path)?;
        file.write_all(text.expose().as_bytes())
    }

    /// The environment of a sign-in.
    pub fn login_env(&self) -> Vec<(String, String)> {
        let mut env = self.service_env();
        env.push(("LITE_ARGS_FILE".to_string(), "/data/.login-args".to_string()));
        env
    }

    /// Stop a sign-in that is still running: SIGINT to the launcher (it ends its child), then
    /// force.
    pub fn end_launcher(&self, child: &mut Child) {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        let system = &self.context.system;
        let pid = child.id() as i32;
        let targets: Vec<(i32, Option<u64>)> =
            system.descendants(pid).into_iter().map(|target| (target, system.proc_start_time(target))).collect();
        // not collected yet (try_wait() above), so the number is still this launcher's
        signal_process(pid, libc::SIGINT);
        if !self.wait_child_quietly(child, Duration::from_secs(5)) {
            let _ = child.kill();
            let _ = child.wait();
        }
        for (target, started) in targets {
            // still the same process
            if started.is_some() && system.proc_start_time(target) == started {
                signal_process(target, libc::SIGKILL);
            }
        }
    }

    /// One line from a descriptor, without its line end; `None` at the end of the input. Reads
    /// byte by byte: what follows the line belongs to whoever reads next (the wrapper).
    fn read_line(&self, fd: BorrowedFd<'_>) -> Result<Option<Vec<u8>>, HelperError> {
        let mut line = Vec::with_capacity(4096);
        let mut byte = [0u8; 1];
        loop {
            if let Err(error) = self.wait_readable(&[fd], None) {
                wipe(&mut line);
                return Err(error);
            }
            match read_some(fd, &mut byte) {
                Ok(0) if line.is_empty() => return Ok(None),
                Ok(0) => return Ok(Some(line)),
                Ok(_) if byte[0] == b'\n' => {
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    return Ok(Some(line));
                }
                Ok(_) => line.push(byte[0]),
                Err(error) => {
                    wipe(&mut line);
                    return Err(HelperError::io("reading the answer", error));
                }
            }
        }
    }

    /// `input()`: the prompt on standard error (where Python writes it in a terminal), the
    /// answer from standard input.
    fn input(&self, prompt: &str) -> Result<String, HelperError> {
        let _ = write_all(self.context.stderr.as_fd(), prompt.as_bytes());
        let line = self.read_line(self.context.stdin.as_fd())?;
        let line = line.ok_or_else(|| HelperError::failed("EOFError", "EOF when reading a line"))?;
        String::from_utf8(line).map_err(|_| HelperError::failed("UnicodeDecodeError", "the answer is not valid UTF-8"))
    }

    /// `getpass.getpass()`: the answer is read with the terminal's echo off, on the controlling
    /// terminal when there is one, else on standard input with the prompt on standard error.
    fn getpass(&self, prompt: &str) -> Result<Secret, HelperError> {
        let context = self.context;
        let terminal = context
            .controlling_terminal
            .as_ref()
            .and_then(|path| OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOCTTY).open(path).ok());
        let (input, output) = match &terminal {
            Some(terminal) => (terminal.as_fd(), terminal.as_fd()),
            None => (context.stdin.as_fd(), context.stderr.as_fd()),
        };
        let echo = |error| HelperError::io("turning off the echo for the password", error);
        let saved = terminal_attributes(input).map_err(echo)?;
        let mut hidden = saved;
        hidden.c_lflag &= !libc::ECHO;
        // TCSAFLUSH: what was typed before the prompt is not the password
        set_terminal_attributes(input, libc::TCSAFLUSH, &hidden).map_err(echo)?;
        let echo_back = EchoBack { fd: input, saved };
        let _ = write_all(output, prompt.as_bytes());
        let line = self.read_line(input);
        drop(echo_back);
        let line = line?.ok_or_else(|| HelperError::failed("EOFError", "EOF when reading a line"))?;
        let _ = write_all(output, b"\n");
        Secret::from_utf8(line)
    }

    pub fn cmd_login(&self, explain_only: bool, pipe: bool, stop_service: bool) -> Result<i32, HelperError> {
        if pipe {
            return Ok(self.login_pipe(stop_service));
        }
        self.require_build()?;
        self.require_runtime()?;
        let paths = self.paths();
        let notice = LOGIN_NOTICE.replace("{data}", &paths.data.display().to_string());
        if explain_only {
            self.say(&notice)?;
            return Ok(0);
        }
        self.require_one_answer_wrapper()?;
        self.require_startable()?;
        if !self.context.stdin_is_terminal || !self.context.stdout_is_terminal {
            return Err(HelperError::refused(
                "run the login from your own terminal; never send passwords or codes through chat",
            ));
        }
        let _lock = self.helper_lock()?;
        let args_path = self.login_preflight(stop_service, false)?;
        self.say(&notice)?;
        let username = py_strip(&self.input("Apple account email: ")?).to_string();
        let password = self.getpass("Apple account password (hidden): ")?;
        check_credentials(&username, password.expose())?;

        let started_ns = self.now_ns();
        let mut attempt = Attempt::new(self, &args_path);
        let result = self.terminal_attempt(&mut attempt, &username, password, started_ns);
        let result = match result {
            Err(HelperError::Interrupted) => {
                self.warn("\nlogin interrupted");
                Ok(130)
            }
            other => other,
        };
        drop(attempt);
        result
    }

    fn terminal_attempt(
        &self,
        attempt: &mut Attempt<'_, '_>,
        username: &str,
        password: Secret,
        started_ns: u128,
    ) -> Result<i32, HelperError> {
        let data = &self.paths().data;
        self.keep_account_files().map_err(|error| HelperError::io(data.display(), error))?;
        self.write_login_args(attempt.args_path, username, &password)
            .map_err(|error| HelperError::io(attempt.args_path.display(), error))?;
        drop(password);
        self.say("\nHanding over to the wrapper. Its own messages follow; answer \"2FA code:\" when it appears.\n")?;
        let child = self
            .spawn_launcher(&[], &self.login_env(), true, None)
            .map_err(|error| HelperError::io(self.paths().launcher.display(), error))?;
        let status = self.wait_child(attempt.child.insert(child))?;
        // The upstream host launcher returns 0 even when the wrapper failed, so the token check decides.
        attempt.succeeded = status.code() == Some(0) && self.fresh_account_files(started_ns);
        if !attempt.succeeded {
            return Err(HelperError::refused(
                "login did not produce fresh account tokens; read the wrapper's messages above. An earlier sign-in \
                 on this machine, if any, was kept.",
            ));
        }
        self.say(
            "\nLogin completed: fresh account token files exist in the private runtime (contents not shown). am-ui \
             uses this sign-in from now on and starts the lossless service when it needs it.",
        )?;
        Ok(0)
    }

    // ------------------------------------------------------------------------- login --pipe

    /// Keep the scrubbed lines of a failed sign-in for the person to read (0600); the path,
    /// `None` when it could not be written.
    fn write_signin_log(
        &self,
        outcome: &str,
        steps: &[String],
        lines: &VecDeque<String>,
        exit_status: Option<i32>,
    ) -> Option<String> {
        let paths = self.paths();
        let launcher = match exit_status {
            None => "still running, stopped by the helper".to_string(),
            Some(status) => format!("exit status {status}"),
        };
        let mut text = format!(
            "am-ui sign-in log: what Apple's sign-in component printed during the last failed sign-in.\n\
             Left out: the Apple ID, e-mail addresses, the password, the verification code, token-like strings\n\
             and long numbers. Apple's own messages are kept as they came: read it before you share it.\n\
             when: {}\n\
             result: {outcome}\n\
             steps: {}\n\
             launcher: {launcher}\n\
             --- output ({} line{}) ---\n",
            utc_stamp(self.context.clock.now()),
            if steps.is_empty() { "(none)".to_string() } else { steps.join(", ") },
            lines.len(),
            if lines.len() == 1 { "" } else { "s" },
        );
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        util::create_private_dirs(&paths.logs).ok()?;
        remove_file_if_exists(&paths.signin_log).ok()?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&paths.signin_log)
            .ok()?;
        file.write_all(text.as_bytes()).ok()?;
        Some(paths.signin_log.display().to_string())
    }

    /// The sign-in for the am-ui window: same wrapper, same checks, no terminal.
    ///
    /// stdin, one JSON object per line:
    ///
    /// ```text
    ///   {"username": "...", "password": "..."}   first, within a minute
    ///   {"code": "123456"}                       after the "code_required" phase
    ///   {"cancel": true}                         any time; closing stdin cancels too
    /// ```
    ///
    /// stdout, one JSON object per line:
    ///
    /// ```text
    ///   {"event": "phase", "phase": "preparing" | "signing_in" | "code_required" | "verifying" | "finishing"}
    ///   {"event": "notice", "error": "..."}      a rejected input line (the sign-in goes on)
    ///   {"event": "result", "ok": true}          fresh account files exist; exit status 0
    ///   {"event": "result", "ok": false, "kind": "...", "error": "...", "steps": [...], "log": "..."}
    ///                                            exit status 1; "log" (a path) when one was written
    /// ```
    ///
    /// kinds: input, unavailable, busy, credentials, code, apple, cancelled, timeout, failed.
    ///
    /// The wrapper runs on a pseudo-terminal so that it asks for the code with its terminal
    /// prompt (which waits as long as needed) and never sees this helper's pipes. What it prints
    /// is read here and matched against a few fixed markers; its last lines are kept, scrubbed,
    /// for the log of a failed attempt and are never passed on as they came: Apple's dialog
    /// texts can name the account. Before anything else the wrapper is started without an
    /// account (launcher_selftest): where the system does not let it run, the attempt ends as
    /// "unavailable" with the reason before the password is used. Two guards against Apple's
    /// lockout: a second password request means Apple refused the first one, and a second code
    /// request means it refused the code; either ends the attempt instead of answering again.
    pub fn login_pipe(&self, stop_service: bool) -> i32 {
        let mut steps: Vec<String> = Vec::new();
        // end like a cancel: the clean-up must run
        self.context.interrupt.watch(&[libc::SIGINT, libc::SIGTERM, libc::SIGHUP]);
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.pipe_sign_in(stop_service, &mut steps)));
        let failure = match outcome {
            Ok(Ok(())) => return 0,
            Ok(Err(failure)) => failure,
            // the window must always get a result line
            Err(_) => PipeFailure::new("failed", "The sign-in helper failed (panic)"),
        };
        let mut event = json!({
            "event": "result",
            "ok": false,
            "kind": failure.kind,
            "error": failure.message,
            "steps": steps,
        });
        if let (Some(log), Some(event)) = (failure.log, event.as_object_mut()) {
            event.insert("log".to_string(), Value::String(log));
        }
        self.emit(&event);
        1
    }

    fn pipe_sign_in(&self, stop_service: bool, steps: &mut Vec<String>) -> Result<(), PipeFailure> {
        let mut control = LineInput::new(self.context.stdin.as_fd());
        if self.context.stdin_is_terminal {
            return Err(PipeFailure::new(
                "input",
                "--pipe is how the am-ui window signs in; in a terminal run tools/lossless/login.sh",
            ));
        }
        let unavailable = |error| PipeFailure::from_check(error, "unavailable");
        self.require_build().map_err(unavailable)?;
        self.require_runtime().map_err(unavailable)?;
        self.require_one_answer_wrapper().map_err(unavailable)?;
        self.require_startable().map_err(unavailable)?;
        self.emit_phase("preparing");
        let first = control.next_message(self, PIPE_CREDENTIALS_TIMEOUT_S).map_err(unavailable)?;
        let Some(mut first) = first.filter(|first| !truthy(first.get("cancel"))) else {
            return Err(PipeFailure::cancelled());
        };
        let (username, password) = match (first.remove("username"), first.remove("password")) {
            (Some(Value::String(username)), Some(Value::String(password))) => (username, Secret::new(password)),
            _ => return Err(PipeFailure::new("input", "an email and a password are required")),
        };
        if control.cancel_followed {
            return Err(PipeFailure::cancelled());
        }
        let username = py_strip(&username).to_string();
        check_credentials(&username, password.expose()).map_err(|error| PipeFailure::from_check(error, "input"))?;
        let busy = |error| PipeFailure::from_check(error, "busy");
        let _lock = self.helper_lock().map_err(busy)?;
        let args_path = self.login_preflight(stop_service, true).map_err(busy)?;
        self.run_pipe_login(&mut control, &args_path, &username, password, steps)
    }

    /// Why a sign-in that ran to its end left no account files, from what the wrapper reported.
    fn failure_after_exit(&self, seen: &Seen, lines: &VecDeque<String>) -> PipeFailure {
        if let Some(message) = &seen.apple_message {
            return PipeFailure::says_it_all("apple", message.clone());
        }
        let code = seen.auth_code.as_deref().filter(|code| !code.is_empty());
        if let Some(dialog) = &seen.dialog {
            let error = code.map(|code| format!(" (error {code})")).unwrap_or_default();
            return PipeFailure::new("apple", format!("Apple answered: {dialog}{error}"));
        }
        if let Some(code) = code {
            let detail = seen.auth_message.as_ref().map(|detail| format!(": {detail}")).unwrap_or_default();
            return PipeFailure::new("apple", format!("Apple refused the sign-in (error {code}{detail})"));
        }
        if seen.signed_in {
            let what = match seen.token_problem {
                Some(what) => format!("{what} could not be fetched afterwards"),
                None => "its token request failed".to_string(),
            };
            return PipeFailure::new("failed", format!("Apple accepted the sign-in but {what}; try again later"));
        }
        if let Some(response_type) = &seen.response_type {
            return PipeFailure::new(
                "apple",
                format!("Apple ended the sign-in without giving a reason (response type {response_type})"),
            );
        }
        if !seen.started {
            let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
            return PipeFailure::new(
                "unavailable",
                format!("Apple's sign-in component did not start: {}", self.explain_start_failure(&lines)),
            );
        }
        if let Some(problem) = &seen.tls_problem {
            return PipeFailure::new("failed", format!("Apple's sign-in component refused to connect: {problem}"));
        }
        if !seen.contacting {
            return PipeFailure::new(
                "failed",
                "Apple's sign-in component stopped while starting up, before it contacted Apple",
            );
        }
        if seen.login_failed {
            return PipeFailure::new(
                "failed",
                "The sign-in ended without an answer from Apple; that is usually the network (a VPN or a DNS filter \
                 can block it)",
            );
        }
        PipeFailure::new("failed", "Apple's sign-in component stopped unexpectedly while signing in")
    }

    fn run_pipe_login(
        &self,
        control: &mut LineInput<'_>,
        args_path: &Path,
        username: &str,
        password: Secret,
        steps: &mut Vec<String>,
    ) -> Result<(), PipeFailure> {
        let started_ns = self.now_ns();
        let mut attempt = Attempt::new(self, args_path);
        let mut session = PipeSession {
            helper: self,
            args_path,
            username,
            typed: vec![password], // never in the log: the password, and the code once it is sent
            lines: VecDeque::with_capacity(SIGNIN_LOG_LINES),
            seen: Seen::default(),
            steps,
            waiting_for_code: false,
            code_sent: false,
            password_requests: 0,
        };
        let outcome = session.run(control, &mut attempt, started_ns);
        let shown = self.shown_path(&self.paths().signin_log);
        let result = match outcome {
            Ok(()) => Ok(()),
            Err(PipeStop::Failure(mut failure)) => {
                if failure.kind != "cancelled" {
                    let outcome = format!("{}: {}", failure.kind, failure.message);
                    failure.log = self.write_signin_log(&outcome, session.steps, &session.lines, attempt.exit_status());
                    if failure.log.is_some() && !failure.says_it_all {
                        failure.message = format!("{}. Details: {shown}", failure.message.trim_end_matches('.'));
                    }
                }
                Err(failure)
            }
            // reported like any failed attempt, with the log
            Err(PipeStop::Unexpected(name)) => {
                let mut failure = PipeFailure::new("failed", format!("The sign-in helper failed ({name})"));
                let outcome = format!("failed: helper error {name}");
                failure.log = self.write_signin_log(&outcome, session.steps, &session.lines, attempt.exit_status());
                if failure.log.is_some() {
                    failure.message = format!("{}. Details: {shown}", failure.message);
                }
                Err(failure)
            }
        };
        drop(session);
        drop(attempt);
        result
    }

    /// Run one command.
    fn command(&self, invocation: Invocation) -> Result<i32, HelperError> {
        match invocation {
            Invocation::Paths { sh } => self.cmd_paths(sh),
            Invocation::Fingerprint => self.cmd_fingerprint(),
            Invocation::BuildState => self.cmd_build_state(),
            Invocation::Stage => self.cmd_stage(),
            Invocation::Start => self.cmd_start(),
            Invocation::Stop => self.cmd_stop(),
            Invocation::Status => self.cmd_status(),
            Invocation::Selftest => self.cmd_selftest(),
            Invocation::Login { explain_only, pipe, stop_service } => self.cmd_login(explain_only, pipe, stop_service),
        }
    }
}

/// The service after `start`: it goes on by itself, whichever way the command ends.
struct Service(Option<Child>);

impl Drop for Service {
    fn drop(&mut self) {
        if let Some(child) = self.0.take() {
            detach(child);
        }
    }
}

/// The launcher's argument list; it is started with the launcher file as the program.
pub fn launcher_command(args: &[&str]) -> Vec<String> {
    [LAUNCHER_ARGV0, "--base-dir", "/data"].iter().chain(args).map(|arg| arg.to_string()).collect()
}

fn base_env() -> Vec<(String, String)> {
    [("PATH", "/system/bin"), ("HOME", "/data"), ("LANG", "C.UTF-8")]
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// A `--pipe` sign-in while the wrapper runs.
struct PipeSession<'h, 'a> {
    helper: &'h Helper<'a>,
    args_path: &'h Path,
    username: &'h str,
    typed: Vec<Secret>,
    /// The wrapper's output, scrubbed.
    lines: VecDeque<String>,
    seen: Seen,
    steps: &'h mut Vec<String>,
    waiting_for_code: bool,
    code_sent: bool,
    password_requests: u32,
}

impl PipeSession<'_, '_> {
    fn step(&mut self, name: &str) {
        if self.steps.last().map(String::as_str) != Some(name) {
            self.steps.push(name.to_string());
        }
    }

    fn ask_for_code(&mut self) -> Result<(), PipeFailure> {
        if self.waiting_for_code {
            return Ok(());
        }
        if self.code_sent {
            return Err(PipeFailure::code_refused());
        }
        self.waiting_for_code = true;
        self.step("code_prompt");
        self.helper.emit_phase("code_required");
        Ok(())
    }

    fn on_line(&mut self, line: &str) -> Result<(), PipeStop> {
        let typed: Vec<&str> = self.typed.iter().map(Secret::expose).collect();
        if self.lines.len() == SIGNIN_LOG_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(scrub_line(line, self.username, &typed));
        if line.contains("wrapper-lite login mode") || line.contains("logging in...") {
            remove_file_if_exists(self.args_path)?; // the wrapper has read it
            self.seen.started = true;
            if line.contains("logging in...") {
                self.seen.contacting = true;
                self.step("signing_in");
                self.helper.emit_phase("signing_in");
            }
        } else if line.contains("credentialHandler:") {
            if line.contains("2FA: true") {
                self.step("code_request");
                if self.code_sent {
                    return Err(PipeFailure::code_refused().into());
                }
            } else {
                self.step("password_request");
                self.password_requests += 1;
                if self.password_requests > 1 {
                    return Err(PipeFailure::says_it_all(
                        "credentials",
                        "Apple did not accept the Apple ID or password. Check them before trying again: Apple locks \
                         an account after a few wrong passwords.",
                    )
                    .into());
                }
            }
        } else if line.contains("dialogHandler:") {
            // "Sign In" is the dialog the wrapper answers by itself
            if let Some(found) = DIALOG_RE.captures(line)
                && let title = py_strip(&found[1])
                && !title.is_empty()
                && title != "Sign In"
            {
                let message = py_strip(&found[2]);
                let message = py_strip(message.strip_suffix('}').unwrap_or(message));
                let text = if message.is_empty() { title.to_string() } else { format!("{title}: {message}") };
                self.seen.dialog = Some(safe_apple_message(&text, self.username));
                self.step("dialog");
            }
        } else if line.contains("Enter your 2FA code into") || line.contains("2FA code timeout") {
            return Err(PipeFailure::new(
                "failed",
                "The wrapper did not take the code from this helper; sign in from a terminal instead: \
                 tools/lossless/login.sh",
            )
            .into());
        } else if line.contains("2FA code:") {
            self.ask_for_code()?; // the prompt, with a log line of another thread printed after it
        } else if let Some((_, message)) = line.split_once("server message:") {
            self.seen.apple_message = Some(safe_apple_message(message, self.username));
            self.step("apple_message");
        } else if line.contains("auth error: code=") {
            if let Some(found) = AUTH_ERROR_RE.captures(line) {
                self.seen.auth_code = Some(found[1].to_string());
                let detail = safe_apple_message(found.get(2).map_or("", |detail| detail.as_str()), self.username);
                if !detail.is_empty() && detail != "none" {
                    self.seen.auth_message = Some(detail);
                }
            }
            self.step("auth_error");
        } else if line.contains("auth failed: response type") {
            let response_type = RESPONSE_TYPE_RE.captures(line).map(|found| found[1].to_string());
            self.seen.response_type = Some(response_type.unwrap_or_else(|| "unknown".to_string()));
            self.step("auth_failed");
        } else if let Some((_, policy)) = line.split_once("tls policy:")
            && (line.contains("refusing") || line.contains("missing"))
        {
            self.seen.tls_problem = Some(printable(policy, 200));
            self.step("tls_problem");
        } else if line.contains("login successful") {
            self.seen.signed_in = true;
            self.step("signed_in");
            self.helper.emit_phase("finishing");
        } else if line.contains("login failed") {
            self.seen.login_failed = true;
            self.step("login_failed");
        } else if line.contains("login complete, exiting") {
            self.step("complete");
        } else if line.contains("devToken error")
            || line.contains("devtoken:")
            || line.contains("failed to get dev token")
        {
            self.seen.token_problem = Some("the developer token");
            self.step("token_problem");
        } else if line.contains("createMusicToken") || line.contains("failed to get music token") {
            self.seen.token_problem = Some("the music token");
            self.step("token_problem");
        } else if line.contains("failed to get storefront ID") {
            self.seen.token_problem = Some("the storefront");
            self.step("token_problem");
        }
        Ok(())
    }

    /// What the window sent while the wrapper runs.
    fn on_messages(&mut self, messages: Vec<Map<String, Value>>, master: BorrowedFd<'_>) -> Result<bool, PipeStop> {
        let mut answered = false;
        for mut message in messages {
            if truthy(message.get("cancel")) {
                return Err(PipeFailure::cancelled().into());
            }
            let Some(Value::String(code)) = message.remove("code").filter(|_| self.waiting_for_code) else {
                continue;
            };
            let code = Secret::new(code);
            if code.expose().len() != 6 || !code.expose().bytes().all(|digit| digit.is_ascii_digit()) {
                self.helper.emit(&json!({"event": "notice", "error": "The verification code is six digits"}));
                continue;
            }
            let mut line = [b'\n'; 7];
            line[..6].copy_from_slice(code.expose().as_bytes());
            let written = write_all(master, &line);
            wipe(&mut line);
            self.typed.push(code);
            written?;
            self.waiting_for_code = false;
            self.code_sent = true;
            answered = true;
            self.step("code_sent");
            self.helper.emit_phase("verifying");
        }
        Ok(answered)
    }

    fn run(
        &mut self,
        control: &mut LineInput<'_>,
        attempt: &mut Attempt<'_, '_>,
        started_ns: u128,
    ) -> Result<(), PipeStop> {
        let helper = self.helper;
        helper.keep_account_files()?;
        helper.write_login_args(self.args_path, self.username, &self.typed[0])?;
        let (master, slave) = open_pty()?;
        let master = AsFd::as_fd(&*attempt.master.insert(master));
        let mut attributes = terminal_attributes(slave.as_fd())?;
        attributes.c_lflag &= !(libc::ECHO | libc::ECHONL); // the code is not echoed back to us
        set_terminal_attributes(slave.as_fd(), libc::TCSANOW, &attributes)?;
        let launch = Launch {
            program: helper.paths().launcher.clone(),
            argv: launcher_command(&[]),
            cwd: helper.paths().runtime.clone(),
            env: helper.login_env(),
            stdin: Stdio::from(slave.try_clone()?),
            stdout: Stdio::from(slave.try_clone()?),
            stderr: Stdio::from(slave),
            new_session: true,
        };
        let child = attempt.child.insert(helper.context.system.spawn(launch)?);

        let mut pending: Vec<u8> = Vec::new(); // wrapper output not yet ended by a newline
        let mut chunk = [0u8; 4096];
        let mut quiet_since = helper.monotonic();
        loop {
            let ready = match control.closed {
                true => helper.wait_readable(&[master], Some(1.0))?,
                false => helper.wait_readable(&[master, control.fd], Some(1.0))?,
            };
            if ready[0] {
                let count = match read_some(master, &mut chunk) {
                    Ok(count) => count,
                    // EIO: every process on the terminal has exited
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => 0,
                    Err(error) => return Err(error.into()),
                };
                if count == 0 {
                    break;
                }
                quiet_since = helper.monotonic();
                pending.extend_from_slice(&chunk[..count]);
                while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                    let raw: Vec<u8> = pending.drain(..=end).collect();
                    self.on_line(String::from_utf8_lossy(&raw[..end]).trim_end_matches('\r'))?;
                }
                let prompt = pending.iter().rposition(|byte| !is_py_space_byte(*byte)).map_or(0, |last| last + 1);
                if pending[..prompt].ends_with(b"2FA code:") {
                    pending.clear(); // the prompt ends without a newline
                    self.ask_for_code()?;
                } else if pending.len() > 65536 {
                    pending.clear();
                }
            }
            if ready.get(1).copied().unwrap_or(false) && self.on_messages(control.feed(), master)? {
                quiet_since = helper.monotonic();
            }
            if control.closed {
                return Err(PipeFailure::cancelled().into());
            }
            let quiet = helper.monotonic() - quiet_since;
            if self.waiting_for_code && quiet > PIPE_CODE_TIMEOUT_S {
                return Err(PipeFailure::says_it_all(
                    "timeout",
                    "No verification code was entered in time; start again",
                )
                .into());
            }
            if !self.waiting_for_code && quiet > PIPE_QUIET_TIMEOUT_S {
                return Err(PipeFailure::new(
                    "timeout",
                    "The sign-in did not progress; check the network and start again",
                )
                .into());
            }
        }

        if pending.iter().any(|byte| !is_py_space_byte(*byte)) {
            self.on_line(String::from_utf8_lossy(&pending).trim_end_matches('\r'))?;
        }
        helper.wait_child_until(child, helper.monotonic() + 15.0)?;
        attempt.succeeded = helper.fresh_account_files(started_ns);
        if attempt.succeeded {
            remove_file_if_exists(&helper.paths().signin_log)?; // the log of an earlier failed attempt
            helper.emit(&json!({"event": "result", "ok": true}));
            return Ok(());
        }
        Err(helper.failure_after_exit(&self.seen, &self.lines).into())
    }
}

/// Run one command of the helper: what `python3 wrapperctl.py ARGS` does, with everything it
/// touches taken from `context`. Returns the exit status.
pub fn run(args: &[String], context: &Context) -> i32 {
    let helper = Helper::new(context);
    let invocation = match parse_args(args, &context.program) {
        Parsed::Run(invocation) => invocation,
        Parsed::Exit { status, stdout, stderr } => {
            let _ = write_all(context.stdout.as_fd(), stdout.as_bytes());
            let _ = write_all(context.stderr.as_fd(), stderr.as_bytes());
            return status;
        }
    };
    match helper.command(invocation) {
        Ok(status) => status,
        Err(HelperError::Interrupted) => 130,
        Err(error) => {
            helper.warn(&format!("error: {error}"));
            1
        }
    }
}

/// The helper as a program, what `python3 wrapperctl.py ARGS` was: `program` is the name its
/// usage lines show (how it was called), `args` are its arguments. Like the script it works with
/// umask 077 and reports a closed pipe as an error instead of dying of it. The result is the
/// exit status: a program that ends with `std::process::exit(wrapper::main(..))` is the helper.
pub fn main(program: &str, args: &[String]) -> i32 {
    // SAFETY: changes the disposition of one signal and the umask of this process, which is the
    // helper and nothing else.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::umask(0o077);
    }
    match Context::from_process() {
        Ok(mut context) => {
            context.program = program.to_string();
            run(args, &context)
        }
        Err(error) => {
            eprintln!("error: {}", strerror(&error));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn json_is_written_the_way_python_writes_it() {
        let value =
            json!({"b": [1, {"z": null, "a": "\u{e9}\u{1f600}\u{7f}\n\"\\"}], "a": {}, "c": [], "d": 1.5, "e": true});
        let escaped = "\\u00e9\\ud83d\\ude00\\u007f\\n\\\"\\\\";
        assert_eq!(
            json_dumps(&value, None),
            format!(
                "{{\"b\": [1, {{\"z\": null, \"a\": \"{escaped}\"}}], \"a\": {{}}, \"c\": [], \"d\": 1.5, \"e\": true}}"
            )
        );
        assert_eq!(
            json_dumps(&value, Some(2)),
            "{\n  \"b\": [\n    1,\n    {\n      \"z\": null,\n      \"a\": \"\\u00e9\\ud83d\\ude00\\u007f\\n\\\"\\\\\"\n    }\n  ],\n  \
             \"a\": {},\n  \"c\": [],\n  \"d\": 1.5,\n  \"e\": true\n}"
        );
        assert_eq!(json_dumps(&json!("/tmp/a b"), None), "\"/tmp/a b\"");
    }

    #[test]
    fn floats_are_written_the_way_python_writes_them() {
        for (value, text) in [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (1.5, "1.5"),
            (123.456, "123.456"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-7, "1.5e-07"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1.2345e22, "1.2345e+22"),
            (-2.5e-10, "-2.5e-10"),
            (0.1, "0.1"),
            (1e100, "1e+100"),
        ] {
            assert_eq!(py_float_repr(value), text);
        }
    }

    #[test]
    fn text_helpers_match_python() {
        assert_eq!(py_splitlines("a\nb\r\nc\rd\x0be\u{2028}f\n"), ["a", "b", "c", "d", "e", "f"]);
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
        assert_eq!(py_splitlines("\n\nx"), ["", "", "x"]);
        assert_eq!(py_strip(" \t\x1f a b \u{a0}\n"), "a b");
        assert_eq!(py_split(" a\tb\x1cc  d ").collect::<Vec<_>>(), ["a", "b", "c", "d"]);
        assert_eq!(py_repr("abc"), "'abc'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a'b\"c\\\n\x00\u{e9}\u{200b}\u{1f600}"), "'a\\'b\"c\\\\\\n\\x00\u{e9}\\u200b\u{1f600}'");
        assert_eq!(printable("  a\tb\x00c\u{a0}d\u{200b}e\u{e000}f  \u{1f600} ", 200), "a b c d e f \u{1f600}");
        assert_eq!(printable("abcdef", 3), "abc");
        assert_eq!(printable_only("a\x1b[31mb"), "a [31mb");
        assert!(truthy(Some(&json!(true))) && truthy(Some(&json!("x"))) && truthy(Some(&json!(1))));
        assert!(!truthy(None) && !truthy(Some(&json!(null))) && !truthy(Some(&json!(0))) && !truthy(Some(&json!(""))));
        assert_eq!(count_occurrences(b"aXXaXXXa", b"XX"), 2);
    }

    #[test]
    fn addresses_are_read_the_way_python_reads_them() {
        for (text, address) in [
            ("192.0.2.53", Some("192.0.2.53")),
            ("2001:DB8:0:0::1", Some("2001:db8::1")),
            ("fe80::1%eth0", Some("fe80::1%eth0")),
            ("::ffff:192.0.2.1", Some("::ffff:192.0.2.1")),
            ("192.0.2.053", None),
            ("192.0.2", None),
            ("192.0.2.1%eth0", None),
            ("fe80::1%", None),
            ("resolver.example", None),
            ("", None),
        ] {
            assert_eq!(ip_address(text).as_deref(), address, "{text}");
        }
    }

    #[test]
    fn paths_are_spelled_the_way_pathlib_spells_them() {
        assert_eq!(clean(Path::new("/a//b/./c/")), PathBuf::from("/a/b/c"));
        assert_eq!(clean(Path::new("./a/../b")), PathBuf::from("a/../b"));
        assert_eq!(clean(Path::new("")), PathBuf::from("."));
        let folder = tempfile::tempdir().unwrap();
        let real = folder.path().canonicalize().unwrap();
        fs::create_dir(real.join("target")).unwrap();
        std::os::unix::fs::symlink(real.join("target"), real.join("link")).unwrap();
        assert_eq!(realpath(&real.join("link/missing/file")), real.join("target/missing/file"));
        assert_eq!(realpath(&real.join("target")), real.join("target"));
    }

    #[test]
    fn time_stamps_have_the_script_s_form() {
        let moment = UNIX_EPOCH + Duration::from_secs(1_767_225_600); // 2026-01-01T00:00:00Z
        assert_eq!(utc_stamp(moment), "2026-01-01T00:00:00Z");
        let local = local_stamp(moment);
        let form = Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}[+-]\d{4}$").unwrap();
        assert!(form.is_match(&local), "{local}");
    }

    #[test]
    fn the_command_line_is_read_like_argparse() {
        let run = |list: &[&str]| parse_args(&args(list), "wrapperctl.py");
        assert_eq!(run(&["paths"]), Parsed::Run(Invocation::Paths { sh: false }));
        assert_eq!(run(&["paths", "--sh"]), Parsed::Run(Invocation::Paths { sh: true }));
        assert_eq!(run(&["--", "paths", "--s"]), Parsed::Run(Invocation::Paths { sh: true }));
        assert_eq!(run(&["build-state"]), Parsed::Run(Invocation::BuildState));
        assert_eq!(
            run(&["login", "--pipe", "--stop-service"]),
            Parsed::Run(Invocation::Login { explain_only: false, pipe: true, stop_service: true })
        );
        assert_eq!(
            run(&["login", "--p", "--stop", "--e"]),
            Parsed::Run(Invocation::Login { explain_only: true, pipe: true, stop_service: true })
        );
        let usage = "usage: wrapperctl.py [-h]\n                     \
                     {paths,fingerprint,build-state,stage,start,stop,status,selftest,login} ...\n";
        let error = |message: &str| Parsed::Exit {
            status: 2,
            stdout: String::new(),
            stderr: format!("{usage}wrapperctl.py: error: {message}\n"),
        };
        assert_eq!(run(&[]), error("the following arguments are required: command"));
        assert_eq!(run(&["-x"]), error("the following arguments are required: command"));
        assert_eq!(
            run(&["bogus"]),
            error(
                "argument command: invalid choice: 'bogus' (choose from 'paths', 'fingerprint', 'build-state', \
                 'stage', 'start', 'stop', 'status', 'selftest', 'login')"
            )
        );
        assert_eq!(run(&["login", "--bogus"]), error("unrecognized arguments: --bogus"));
        assert_eq!(run(&["paths", "extra"]), error("unrecognized arguments: extra"));
        assert_eq!(run(&["paths", "--", "--sh"]), error("unrecognized arguments: -- --sh"));
        assert_eq!(run(&["-x", "paths"]), error("unrecognized arguments: -x"));
        assert_eq!(run(&["--sh", "paths"]), error("unrecognized arguments: --sh"));
        assert_eq!(run(&["login", "x", "--pipe"]), error("unrecognized arguments: x"));
        assert_eq!(
            run(&["login", "--pipe=1"]),
            Parsed::Exit {
                status: 2,
                stdout: String::new(),
                stderr: "usage: wrapperctl.py login [-h] [--explain-only] [--pipe] [--stop-service]\n\
                         wrapperctl.py login: error: argument --pipe: ignored explicit argument '1'\n"
                    .to_string(),
            }
        );
        let login_help = "usage: wrapperctl.py login [-h] [--explain-only] [--pipe] [--stop-service]\n\noptions:\n  \
                          -h, --help      show this help message and exit\n  --explain-only  print the notice and \
                          exit\n  --pipe          sign in over JSON lines on stdin/stdout (how the am-ui\n                  \
                          window runs it)\n  --stop-service  stop the service this helper started before signing in\n";
        for list in [&["login", "-h"][..], &["login", "--bogus", "--he"], &["login", "--pipe", "-h", "x"]] {
            assert_eq!(run(list), Parsed::Exit { status: 0, stdout: login_help.to_string(), stderr: String::new() });
        }
        let Parsed::Exit { status: 0, stdout, stderr } = run(&["--he"]) else {
            panic!("--he is --help");
        };
        assert!(stdout.starts_with(usage) && stderr.is_empty());
        assert!(stdout.contains("\n\nam-ui lossless (ALAC) route: control the local FairPlay wrapper service.\n\n"));
        assert!(stdout.ends_with(
            "unless the attempt produced fresh ones.\n\npositional arguments:\n  \
             {paths,fingerprint,build-state,stage,start,stop,status,selftest,login}\n\noptions:\n  -h, --help            \
             show this help message and exit\n"
        ));
        assert_eq!(
            run(&["stage", "-h"]),
            Parsed::Exit {
                status: 0,
                stdout: "usage: wrapperctl.py stage [-h]\n\noptions:\n  -h, --help  show this help message and exit\n"
                    .to_string(),
                stderr: String::new(),
            }
        );
    }

    #[test]
    fn credentials_are_checked_against_the_wrapper_s_limits() {
        let refused = |username: &str, password: &str| match check_credentials(username, password) {
            Err(HelperError::Refused(message)) => message,
            other => panic!("{other:?}"),
        };
        assert!(check_credentials("someone@example.com", "pw with spaces").is_ok());
        assert!(check_credentials("someone@example.com", &"x".repeat(57)).is_ok());
        assert_eq!(refused("", "x"), "an email and a password are required");
        assert_eq!(refused("someone@example.com", ""), "an email and a password are required");
        assert!(refused("a:b@example.com", "x").contains("cannot carry this email address"));
        assert!(refused("a b@example.com", "x").contains("cannot carry this email address"));
        assert!(refused("someone@example.com", "x ").contains("trailing spaces"));
        assert!(refused("someone@example.com", "x\ny").contains("line breaks"));
        assert!(refused("someone@example.com", &"x".repeat(58)).contains("longer than 57 bytes"));
        assert!(refused("someone@example.com", &"é".repeat(29)).contains("63-byte buffer")); // 58 bytes
    }

    #[test]
    fn the_log_keeps_no_account_name_and_nothing_typed() {
        let username = "someone@example.com";
        let typed = ["pw-secret", "654321"];
        assert_eq!(
            scrub_line("\x1b[31m[INFO] Enter the password for Someone@Example.com, 2FA: false}  ", username, &typed),
            "[INFO] Enter the password for <apple id>, 2FA: false}"
        );
        assert_eq!(scrub_line("checking 654321 now", username, &typed), LINE_WITHHELD);
        assert_eq!(scrub_line("submitting pw-secret", username, &typed), LINE_WITHHELD);
        assert_eq!(
            scrub_line(
                &format!("token eyJ{} for other@example.org, account 1234567890", "x".repeat(60)),
                username,
                &typed
            ),
            "token <left out> for <apple id> account <number>"
        );
        assert_eq!(scrub_line(&"y ".repeat(300), "", &[]).chars().count(), 400);
        assert_eq!(scrub_line("nothing typed yet", username, &[""]), "nothing typed yet");
        assert_eq!(
            safe_apple_message(" The Apple ID SOMEONE@example.com cannot\tsign in\x1b[31m right now. ", username),
            "The Apple ID your Apple ID cannot sign in [31m right now."
        );
        assert_eq!(safe_apple_message("write to help@example.org", ""), "write to your Apple ID");
        assert_eq!(safe_apple_message(&"z".repeat(300), username).chars().count(), 240);
    }

    #[test]
    fn secrets_do_not_show_in_debug_output() {
        let secret = Secret::new("pw-secret".to_string());
        assert_eq!(format!("{secret:?}"), "Secret(<hidden>)");
        assert_eq!(secret.expose(), "pw-secret");
        let mut buffer = *b"pw-secret";
        wipe(&mut buffer);
        assert_eq!(buffer, [0; 9]);
        assert!(Secret::from_utf8(vec![0xff, 0xfe]).is_err());
    }

    #[test]
    fn errors_carry_the_system_s_words_and_python_s_names() {
        let missing = io::Error::from_raw_os_error(libc::ENOENT);
        assert_eq!(strerror(&missing), "No such file or directory");
        assert_eq!(python_error_name(&missing), "FileNotFoundError");
        assert_eq!(strerror(&io::Error::other("not started in this test")), "not started in this test");
        assert_eq!(python_error_name(&io::Error::from_raw_os_error(libc::EACCES)), "PermissionError");
        assert_eq!(python_error_name(&io::Error::from_raw_os_error(libc::ENOSPC)), "OSError");
        let error = HelperError::io("/somewhere/lib64", missing);
        assert_eq!(error.to_string(), "/somewhere/lib64: No such file or directory");
        assert_eq!(error.python_name(), "FileNotFoundError");
        assert_eq!(PipeFailure::from_check(HelperError::refused("no"), "busy"), PipeFailure::new("busy", "no"));
        assert_eq!(PipeFailure::from_check(HelperError::Interrupted, "busy").kind, "cancelled");
        assert_eq!(PipeFailure::from_check(error, "busy").message, "The sign-in helper failed (FileNotFoundError)");
    }

    #[test]
    fn the_launcher_runs_under_its_short_name() {
        assert_eq!(launcher_command(&[]), ["wrapper-lite-rootless", "--base-dir", "/data"]);
        assert_eq!(launcher_command(&["--help"]), ["wrapper-lite-rootless", "--base-dir", "/data", "--help"]);
        assert!(LAUNCHER_ARGV0.len() < 128);
    }
}
