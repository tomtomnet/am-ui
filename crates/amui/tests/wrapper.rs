//! The helper of the lossless route (`amui::wrapper`, ported from tools/lossless/wrapperctl.py):
//! the tests of tests/test_signin.py that exercise it, under their names, and tests of the
//! commands those do not reach (stage, start, stop, status, the time limits, the command line).
//!
//! No network, no Apple library and no real account data: the wrapper is a Python stand-in that
//! prints the real wrapper's log lines on the pseudo-terminal it is given, under a scratch
//! AMUI_VAR; tokens, names and passwords are synthetic. The helper runs inside the test process
//! (`wrapper::run` over a `Context` of pipes), and for what only a process can show (signals,
//! the umask) as a child: this test program started again, see `other_roles`.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use amui::config::Config;
use amui::wrapper::{self, Clock, Context, Host, Interrupt, Launch, System, SystemClock};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const USERNAME: &str = "someone@example.com";
/// The arguments of the helper when this test program is started as the helper.
const HELPER_ARGS: &str = "AMUI_TEST_HELPER_ARGS";
/// What the Python script computes for the stand-in tools folder of `ScratchVar`.
const FINGERPRINT: &str = "454593f255bd036eacb098b68723927a18a12134a3a46f879c37b65bba3144ea";

/// The stand-in for var/build/lossless/bin/wrapper-lite-rootless. The password picks the scenario.
const STUB: &str = r##"
import json, os, sys, time
from pathlib import Path

if "--help" in sys.argv[1:]:  # the helper's start check: no account file touched, no report written
    if Path("selftest-fail").exists():
        sys.stderr.write(Path("selftest-fail").read_text())
        sys.exit(1)
    sys.stderr.write("[2026-01-01 00:00:00.000] [INFO] usage: lite [--login user:pass] [--host 127.0.0.1]\n")
    sys.exit(0)
if Path("start-fail").exists():  # a launcher that fails before the wrapper runs
    sys.stderr.write(Path("start-fail").read_text())
    sys.exit(1)

data = Path("rootfs/data")
args_file = data / ".login-args"
lines = args_file.read_text().split("\n") if args_file.exists() else []
username, _, password = (lines[1] if len(lines) > 1 else ":").partition(":")
report = {"argv": sys.argv[1:], "args_lines": len(lines),
          "args_mode": oct(args_file.stat().st_mode & 0o777) if lines else None,
          "tty": [os.isatty(0), os.isatty(1), os.isatty(2)], "args_env": os.environ.get("LITE_ARGS_FILE"),
          "codes": [], "kept": sorted(p.name for p in data.glob("*.before-login")), "pid": os.getpid(),
          "env": sorted(os.environ), "umask": oct(os.umask(0o077)), "own_session": os.getsid(0) == os.getpid()}


def save():
    Path("stub-report.json").write_text(json.dumps(report))


def log(text):
    sys.stderr.write("[2026-01-01 00:00:00.000] [INFO] " + text + "\n")
    sys.stderr.flush()


def ask(suffix=""):
    sys.stdout.write("2FA code: " + suffix)
    sys.stdout.flush()
    code = sys.stdin.readline().strip()
    report["codes"].append(code)
    report["args_at_code"] = args_file.exists()
    save()
    return code


def succeed():
    log("login successful")
    for name in ("DEV_TOKEN", "MUSIC_TOKEN", "STOREFRONT_ID"):
        (data / name).write_text("new-" + name + "\n")
    log("login complete, exiting")


save()
log("tls policy: certificate verification enforced")
log("wrapper-lite login mode")
log("initializing ctx...")
log("logging in...")
for name in ("MUSIC_TOKEN", "STOREFRONT_ID"):
    (data / name).unlink(missing_ok=True)
log("dialogHandler: {title: Sign In, message: Sign in with " + username + "}")
log("credentialHandler: {title: Sign In, message: Enter the password for " + username + ", 2FA: false}")
if password == "pw-plain":
    succeed()
elif password in ("pw-2fa", "pw-2fa-interleaved"):
    log("credentialHandler: {title: Verification, message: code for " + username + ", 2FA: true}")
    if password == "pw-2fa-interleaved":
        code = ask("[2026-01-01 00:00:00.000] [INFO] another thread wrote this\n")
    else:
        code = ask()
    if code == "123456":
        succeed()
    else:  # what the unpatched wrapper does: Apple asks again, the wrapper prompts again
        log("credentialHandler: {title: Verification, message: code for " + username + ", 2FA: true}")
        ask()
        succeed()  # must never be reached: the helper ends the attempt at the second request
elif password == "pw-wrong":
    log("credentialHandler: {title: Sign In, message: Enter the password for " + username + ", 2FA: false}")
    time.sleep(30)
    succeed()  # must never be reached
elif password == "pw-apple":
    log("server message: The Apple ID " + username + " cannot sign in\x1b[31m right now.")
    log("auth error: code=-5000, message=refused")
    log("login failed")
elif password == "pw-token-fail":
    log("login successful")
    log("failed to cache account info")
elif password == "pw-hang":
    time.sleep(60)
elif password == "pw-no-reason":
    log("auth failed: response type 3")
    log("login failed")
elif password == "pw-dialog":
    log("dialogHandler: {title: Verification Failed, message: The account " + username + " needs attention.}")
    log("button 0x7f3ac0: OK")
    log("login failed")
elif password == "pw-no-answer":
    log("login failed")
elif password == "pw-crash":
    sys.exit(1)
elif password == "pw-echo":  # a wrapper that prints what it must not: none of it may reach the log
    log("submitting " + password + " for " + username)
    log("token eyJ" + "x" * 60 + " for other@example.org, account 1234567890")
    log("credentialHandler: {title: Verification, message: code for " + username + ", 2FA: true}")
    code = ask()
    log("checking " + code)
    log("auth failed: response type 5")
    log("login failed")
elif password == "pw-code-file":
    log("credentialHandler: {title: Verification, message: code, 2FA: true}")
    log("Enter your 2FA code into /data/2fa.txt")
    time.sleep(30)
"##;

/// A launcher whose start check never answers: it only says that it runs.
const SILENT_STUB: &str = r##"
import os, time
from pathlib import Path

Path("silent-stub.pid").write_text(str(os.getpid()))
time.sleep(60)
"##;

// ----------------------------------------------------------------------------- other roles

/// This test program in two other roles, decided before the test harness starts:
///
/// * started under the launcher's name (a hard link to this program stands where
///   var/build/lossless/bin/wrapper-lite-rootless is, for the tests that need a launcher /proc
///   shows under that name): the stand-in service of `service_stand_in`;
/// * with `HELPER_ARGS` in the environment: the helper as a program of its own, what
///   `python3 wrapperctl.py ARGS` is for the Python tests.
#[used]
#[unsafe(link_section = ".init_array")]
static OTHER_ROLES: extern "C" fn() = other_roles;

extern "C" fn other_roles() {
    // /proc rather than std::env::args(): this runs before the runtime has collected them
    let Ok(cmdline) = fs::read("/proc/self/cmdline") else {
        return;
    };
    let argv: Vec<String> =
        cmdline.split(|byte| *byte == 0).map(|arg| String::from_utf8_lossy(arg).into_owned()).collect();
    if argv.first().is_some_and(|name| name == wrapper::LAUNCHER_ARGV0) {
        std::process::exit(service_stand_in(&argv[1..argv.len() - 1]));
    }
    if let Ok(args) = std::env::var(HELPER_ARGS) {
        let args: Vec<String> = args.split('\x1f').filter(|arg| !arg.is_empty()).map(String::from).collect();
        std::process::exit(wrapper::main("wrapperctl", &args));
    }
}

/// The launcher as a service: the usage text for `--help`, else a report of how it was started
/// (service-report.json) and an answer to /status on the host and port it is given. A file
/// `service-child` names a shell command it starts beside itself.
fn service_stand_in(args: &[String]) -> i32 {
    if args.iter().any(|arg| arg == "--help") {
        eprintln!("[2026-01-01 00:00:00.000] [INFO] usage: lite [--login user:pass] [--host 127.0.0.1]");
        return 0;
    }
    let value = |name: &str| args.iter().position(|arg| arg == name).and_then(|at| args.get(at + 1)).cloned();
    // SAFETY: reads the umask (by setting it and putting it back) and the session of this process
    let (mask, session) = unsafe {
        let mask = libc::umask(0o077);
        libc::umask(mask);
        (mask, libc::getsid(0))
    };
    let report = json!({
        "argv": args,
        "env": std::env::vars().collect::<BTreeMap<String, String>>(),
        "umask": format!("{mask:o}"),
        "own_session": session == std::process::id() as i32,
        "cwd": std::env::current_dir().ok().and_then(|cwd| cwd.file_name().map(|name| name.to_string_lossy().into_owned())),
    });
    if let Err(error) = fs::write("service-report.json", report.to_string()) {
        eprintln!("service: no report: {error}");
        return 1;
    }
    let (Some(host), Some(port)) = (value("--host"), value("--port")) else {
        eprintln!("service: neither --host nor --port");
        return 2;
    };
    // The test gave the port up a moment ago; a program another test is just starting can still
    // hold a copy of the socket that kept it.
    let bound = Instant::now();
    let listener = loop {
        match TcpListener::bind(format!("{host}:{port}")) {
            Ok(listener) => break listener,
            Err(error) if bound.elapsed() > Duration::from_secs(5) => {
                eprintln!("service: {host}:{port}: {error}");
                return 3;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    if let Ok(script) = fs::read_to_string("service-child") {
        let _ = Command::new("/bin/sh").args(["-c", script.trim()]).spawn();
    }
    println!("service: listening");
    for stream in listener.incoming().flatten() {
        let regions = if Path::new("rootfs/data/MUSIC_TOKEN").exists() { json!(["test"]) } else { json!([]) };
        answer(stream, &json!({"code": 0, "data": {"regions": regions}}));
    }
    0
}

/// Read one HTTP request and answer it with `payload`.
fn answer(mut stream: TcpStream, payload: &Value) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut request = Vec::new();
    let mut chunk = [0u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(count) if count > 0 => request.extend_from_slice(&chunk[..count]),
            _ => return,
        }
    }
    let body = payload.to_string();
    let _ = write!(stream, "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
}

// ----------------------------------------------------------------------------- helpers

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|item| item.to_string()).collect()
}

fn python3() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|folder| folder.join("python3"))
        .find(|candidate| candidate.is_file())
        .expect("python3, for the stand-in launcher")
}

/// Put a script where a program is expected. The file is written by `cp`, not by this process:
/// a file this process had open for writing can still be open in a child another test is just
/// starting, and then it cannot be run ("Text file busy").
fn install_script(path: &Path, content: &str) {
    let source = path.with_extension("source");
    fs::write(&source, content).unwrap();
    let copied = Command::new("cp").arg("--").arg(&source).arg(path).status().unwrap();
    assert!(copied.success());
    fs::remove_file(&source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

fn null() -> OwnedFd {
    File::open("/dev/null").unwrap().into()
}

/// The read end of a pipe nobody writes to: what `input=b""` gives a child process.
fn closed_pipe() -> OwnedFd {
    let (reader, _writer) = io::pipe().unwrap();
    reader.into()
}

fn read_back(mut file: File) -> String {
    let mut text = String::new();
    file.rewind().unwrap();
    file.read_to_string(&mut text).unwrap();
    text
}

fn wait_readable(fd: BorrowedFd<'_>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let mut polled = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let left = deadline.saturating_duration_since(Instant::now()).as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: polls one live descriptor
        match unsafe { libc::poll(&mut polled, 1, left) } {
            ready if ready >= 0 => return ready > 0,
            // a signal (the test run was stopped and continued, say): go on waiting
            _ if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) => {}
            _ => return false,
        }
    }
}

fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

/// A pseudo-terminal: (master, slave), neither inherited by programs the tests start.
fn open_pty() -> (OwnedFd, OwnedFd) {
    // SAFETY: plain libc calls on descriptors this function owns; ptsname_r() ends the name with NUL
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        assert!(master >= 0);
        let master = OwnedFd::from_raw_fd(master);
        let mut name = [0 as libc::c_char; 128];
        assert_eq!(libc::grantpt(master.as_raw_fd()), 0);
        assert_eq!(libc::unlockpt(master.as_raw_fd()), 0);
        assert_eq!(libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()), 0);
        let slave = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        assert!(slave >= 0);
        (master, OwnedFd::from_raw_fd(slave))
    }
}

/// A port on 127.0.0.1 that stays this test's own: bound, and listening only once a test says
/// so. Until then a connection is refused, as on a free port, and nobody else can take it.
fn reserve_port() -> (u16, OwnedFd) {
    // SAFETY: a new socket this function owns, bound to an address structure that lives through the calls
    unsafe {
        let socket = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        assert!(socket >= 0);
        let socket = OwnedFd::from_raw_fd(socket);
        let mut address: libc::sockaddr_in = std::mem::zeroed();
        address.sin_family = libc::AF_INET as libc::sa_family_t;
        address.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
        let mut size = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        assert_eq!(libc::bind(socket.as_raw_fd(), (&raw const address).cast(), size), 0);
        assert_eq!(libc::getsockname(socket.as_raw_fd(), (&raw mut address).cast(), &mut size), 0);
        (u16::from_be(address.sin_port), socket)
    }
}

/// Something that answers /status on the scratch port like a wrapper nobody here started.
struct StatusServer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for StatusServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What a command printed and how it ended.
struct Done {
    code: i32,
    stdout: String,
    stderr: String,
}

/// A scratch am-ui folder with a scratch AMUI_VAR holding a built and staged lossless route
/// whose launcher is the stand-in.
struct ScratchVar {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    var: PathBuf,
    data: PathBuf,
    port: u16,
    port_hold: Mutex<Option<OwnedFd>>,
    env: HashMap<String, String>,
    config: Config,
}

impl ScratchVar {
    fn new() -> ScratchVar {
        // below the target folder: programs can be run from there, whatever /tmp allows
        let dir = tempfile::Builder::new().prefix("signin-test-").tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let root = dir.path().join("am-ui");
        let home = dir.path().join("home");
        let var = root.join("var/tmp/signin-test"); // inside the am-ui folder, as the Python tests have it
        let data = var.join("wrapper/rootfs/data");
        for folder in [
            &home,
            &root.join("workspace"),
            &root.join("tools/lossless/patches"),
            &root.join("tools/lossless/go/amui-fairplay-rip"),
            &var.join("build/lossless/bin"),
            &var.join("build/lossless/wrapper-src/rootfs/system/bin"),
            &var.join("wrapper/rootfs/system/bin"),
            &var.join("wrapper/rootfs/system/etc"),
            &data,
        ] {
            fs::create_dir_all(folder).unwrap();
        }
        fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();
        let tools = root.join("tools/lossless");
        fs::write(tools.join("build.sh"), "#!/bin/sh\n# stand-in\n").unwrap();
        fs::write(tools.join("fairplay-libs.sha256"), format!("# stand-in\n{}  libstand-in.so\n", "0".repeat(64)))
            .unwrap();
        fs::write(tools.join("go/amui-fairplay-rip/main.go"), "package main\n").unwrap();
        fs::write(tools.join("patches/0001-first.patch"), "first\n").unwrap();
        fs::write(tools.join("patches/0002-second.patch"), "second\n").unwrap();
        fs::write(root.join("resolv.conf"), "# stand-in\nnameserver 192.0.2.53\n").unwrap();
        install_script(
            &var.join("build/lossless/bin/wrapper-lite-rootless"),
            &format!("#!{} -I\n{STUB}", python3().display()),
        );
        for path in [
            var.join("build/lossless/wrapper-src/rootfs/system/bin/lite"),
            var.join("wrapper/rootfs/system/bin/lite"),
            var.join("wrapper/rootfs/system/etc/workspace-ca-bundle.pem"),
        ] {
            fs::write(path, "stand-in\nrequested again, aborting login\n").unwrap(); // the mark of patches/0004
        }
        let (port, port_hold) = reserve_port();
        let env: HashMap<String, String> = [
            ("PATH", std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string())),
            ("AMUI_VAR", var.display().to_string()),
            ("LOSSLESS_PORT", port.to_string()),
            ("LANG", "C.UTF-8".to_string()),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
        let config = Config::new(root.clone(), &home, &env);
        ScratchVar { _dir: dir, root, home, var, data, port, port_hold: Mutex::new(Some(port_hold)), env, config }
    }

    /// The helper's surroundings: the scratch folders, and the stand-ins for what it would
    /// read from the host (the resolver list; no trust store).
    fn context(&self, stdin: OwnedFd, stdout: OwnedFd, stderr: OwnedFd) -> Context {
        let mut context = Context::new(&self.config, &self.env, stdin, stdout, stderr).unwrap();
        context.paths.resolv_conf = self.root.join("resolv.conf");
        context.paths.ca_candidates = Vec::new();
        context
    }

    /// Run one command inside this process, standard input being `stdin`.
    fn run_with(&self, args: &[&str], stdin: OwnedFd, adjust: impl FnOnce(&mut Context)) -> Done {
        let (out, err) = (tempfile::tempfile().unwrap(), tempfile::tempfile().unwrap());
        let mut context = self.context(stdin, out.try_clone().unwrap().into(), err.try_clone().unwrap().into());
        adjust(&mut context);
        let code = wrapper::run(&strings(args), &context);
        drop(context);
        Done { code, stdout: read_back(out), stderr: read_back(err) }
    }

    fn run(&self, args: &[&str]) -> Done {
        self.run_with(args, null(), |_| {})
    }

    /// The same command as a program of its own: this test program in its helper role.
    fn process(&self, args: &[&str]) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env_clear()
            .env("PATH", &self.env["PATH"])
            .env("LANG", "C.UTF-8")
            .env("HOME", &self.home)
            .env("AMUI_ROOT", &self.root)
            .env("AMUI_VAR", &self.var)
            .env("LOSSLESS_PORT", self.port.to_string())
            .env("LOSSLESS_DNS", "192.0.2.53")
            .env(HELPER_ARGS, args.join("\x1f"));
        command
    }

    fn earlier_sign_in(&self) {
        for name in ["DEV_TOKEN", "MUSIC_TOKEN", "STOREFRONT_ID"] {
            let path = self.data.join(name);
            fs::write(&path, format!("old-{name}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let file = File::options().write(true).open(&path).unwrap();
            file.set_modified(SystemTime::now() - Duration::from_secs(3600)).unwrap();
        }
    }

    fn account(&self) -> BTreeMap<String, String> {
        let mut files = BTreeMap::new();
        for entry in fs::read_dir(&self.data).unwrap().flatten() {
            if entry.path().is_file() {
                let text = fs::read_to_string(entry.path()).unwrap();
                files.insert(entry.file_name().to_string_lossy().into_owned(), text.trim().to_string());
            }
        }
        files
    }

    fn account_is(&self, expected: &[(&str, &str)]) -> bool {
        self.account() == expected.iter().map(|(name, text)| (name.to_string(), text.to_string())).collect()
    }

    fn old_account(&self) -> bool {
        self.account_is(&[
            ("DEV_TOKEN", "old-DEV_TOKEN"),
            ("MUSIC_TOKEN", "old-MUSIC_TOKEN"),
            ("STOREFRONT_ID", "old-STOREFRONT_ID"),
        ])
    }

    fn runtime(&self, name: &str) -> PathBuf {
        self.var.join("wrapper").join(name)
    }

    /// What `build-state` prints.
    fn state(&self) -> String {
        let done = self.run(&["build-state"]);
        assert_eq!((done.code, done.stderr.as_str()), (0, ""));
        done.stdout.trim().to_string()
    }

    fn report(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.runtime("stub-report.json")).unwrap()).unwrap()
    }

    fn log_file(&self) -> PathBuf {
        self.var.join("wrapper/logs/signin-last.log")
    }

    /// What the launcher of the service wrote, for the message of a failed assertion.
    fn launcher_log(&self) -> String {
        fs::read_to_string(self.var.join("wrapper/logs/launcher.log")).unwrap_or_default()
    }

    /// The log's path as the helper shows it: the scratch folder lies inside the am-ui folder.
    fn log_shown(&self) -> String {
        self.log_file().strip_prefix(&self.root).unwrap().display().to_string()
    }

    /// Answer /status on the scratch port with `payload`, until the result is dropped.
    fn status_server(&self, payload: Value) -> StatusServer {
        let socket = self.port_hold.lock().unwrap().take().expect("the scratch port, still unused");
        // SAFETY: listen() on the bound socket taken over just above
        assert_eq!(unsafe { libc::listen(socket.as_raw_fd(), 16) }, 0);
        let listener = TcpListener::from(socket);
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        answer(stream, &payload);
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        StatusServer { stop, thread: Some(thread) }
    }

    /// Give the scratch port up, for a service that binds it itself.
    fn release_port(&self) {
        self.port_hold.lock().unwrap().take();
    }

    /// Put this test program where the launcher is: a launcher that /proc shows under the
    /// launcher's name, and that serves /status (`service_stand_in`).
    fn install_service_launcher(&self) {
        let launcher = self.var.join("build/lossless/bin/wrapper-lite-rootless");
        fs::remove_file(&launcher).unwrap();
        let program = std::env::current_exe().unwrap();
        if fs::hard_link(&program, &launcher).is_err() {
            assert!(Command::new("cp").arg("--").arg(&program).arg(&launcher).status().unwrap().success());
        }
    }

    /// The pid of the service of the pidfile, when this scratch folder still has one running.
    fn service_pid(&self) -> Option<i32> {
        let text = fs::read_to_string(self.var.join("wrapper/run/service.pid.json")).ok()?;
        serde_json::from_str::<Value>(&text).ok()?.get("pid")?.as_i64().map(|pid| pid as i32)
    }
}

impl Drop for ScratchVar {
    fn drop(&mut self) {
        // a test that failed half-way must not leave its stand-in service behind
        if let Some(pid) = self.service_pid().filter(|pid| *pid > 1) {
            let launcher = self.var.join("build/lossless/bin/wrapper-lite-rootless");
            if fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|program| program == launcher) {
                // SAFETY: signals the process group of the stand-in service this test started
                unsafe { libc::killpg(pid, libc::SIGKILL) };
            }
        }
    }
}

enum Running {
    /// `wrapper::run` on a thread of this process; the exit status arrives here.
    Thread(mpsc::Receiver<i32>),
    /// This test program as the helper.
    Process(Child),
}

/// `login --pipe` with its event lines: the helper inside this process, or as a child.
struct PipeHelper {
    running: Running,
    stdin: Option<File>,
    stdout: File,
    stderr: File,
    interrupt: Option<Arc<Interrupt>>,
    events: Vec<Value>,
    /// The event lines as they were written.
    lines: Vec<String>,
    buffer: Vec<u8>,
}

impl PipeHelper {
    fn start(scratch: &ScratchVar, extra: &[&str]) -> PipeHelper {
        PipeHelper::start_with(scratch, extra, b"", |_| {})
    }

    /// `first` is already in the pipe when the helper starts.
    fn start_with(scratch: &ScratchVar, extra: &[&str], first: &[u8], adjust: impl FnOnce(&mut Context)) -> PipeHelper {
        let (stdin_reader, mut stdin_writer) = io::pipe().unwrap();
        let (stdout_reader, stdout_writer) = io::pipe().unwrap();
        stdin_writer.write_all(first).unwrap();
        let stderr = tempfile::tempfile().unwrap();
        let mut context =
            scratch.context(stdin_reader.into(), stdout_writer.into(), stderr.try_clone().unwrap().into());
        adjust(&mut context);
        let interrupt = context.interrupt.clone();
        let args: Vec<String> = ["login", "--pipe"].iter().chain(extra).map(|arg| arg.to_string()).collect();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let code = wrapper::run(&args, &context);
            drop(context);
            let _ = sender.send(code);
        });
        PipeHelper {
            running: Running::Thread(receiver),
            stdin: Some(File::from(OwnedFd::from(stdin_writer))),
            stdout: File::from(OwnedFd::from(stdout_reader)),
            stderr,
            interrupt: Some(interrupt),
            events: Vec::new(),
            lines: Vec::new(),
            buffer: Vec::new(),
        }
    }

    fn start_process(scratch: &ScratchVar, extra: &[&str]) -> PipeHelper {
        let stderr = tempfile::tempfile().unwrap();
        let args: Vec<&str> = ["login", "--pipe"].iter().chain(extra).copied().collect();
        let mut child = scratch
            .process(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr.try_clone().unwrap())
            .spawn()
            .unwrap();
        PipeHelper {
            stdin: child.stdin.take().map(|stdin| File::from(OwnedFd::from(stdin))),
            stdout: File::from(OwnedFd::from(child.stdout.take().unwrap())),
            stderr,
            running: Running::Process(child),
            interrupt: None,
            events: Vec::new(),
            lines: Vec::new(),
            buffer: Vec::new(),
        }
    }

    fn send(&mut self, message: Value) -> io::Result<()> {
        let stdin = self.stdin.as_mut().expect("the helper's input, still open");
        stdin.write_all(format!("{message}\n").as_bytes())
    }

    fn read_event(&mut self, timeout: Duration) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=end).collect();
                let line = String::from_utf8(line[..end].to_vec()).unwrap();
                let event: Value = serde_json::from_str(&line).unwrap();
                self.lines.push(line);
                self.events.push(event.clone());
                return Some(event);
            }
            let remaining = deadline.checked_duration_since(Instant::now())?;
            if !wait_readable(self.stdout.as_fd(), remaining) {
                return None;
            }
            let mut chunk = [0u8; 4096];
            match self.stdout.read(&mut chunk) {
                Ok(count) if count > 0 => self.buffer.extend_from_slice(&chunk[..count]),
                _ => return None,
            }
        }
    }

    /// Read events up to the named phase, or up to the result.
    fn until(&mut self, phase: Option<&str>) -> Value {
        loop {
            let Some(event) = self.read_event(Duration::from_secs(20)) else {
                panic!("the helper went quiet; events so far: {:?}", self.events);
            };
            if event["event"] == "result" || phase.is_some_and(|phase| event["phase"] == phase) {
                return event;
            }
        }
    }

    /// Close the helper's input and wait for it: (exit status, what it wrote to standard error).
    fn close(&mut self) -> (i32, String) {
        self.stdin.take();
        let code = match &mut self.running {
            Running::Thread(done) => done.recv_timeout(Duration::from_secs(30)).expect("the helper ended"),
            Running::Process(child) => {
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    if let Some(status) = child.try_wait().unwrap() {
                        break status.code().expect("the helper ended by itself");
                    }
                    assert!(Instant::now() < deadline, "the helper did not end");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        };
        (code, read_back(self.stderr.try_clone().unwrap()))
    }

    fn phases(&self) -> Vec<&str> {
        self.events
            .iter()
            .filter(|event| event["event"] == "phase")
            .filter_map(|event| event["phase"].as_str())
            .collect()
    }
}

impl Drop for PipeHelper {
    fn drop(&mut self) {
        self.stdin.take(); // a helper still running takes this as a cancel
        if let Running::Process(child) = &mut self.running
            && matches!(child.try_wait(), Ok(None))
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A finished `login --pipe`.
struct Login {
    helper: PipeHelper,
    result: Value,
    status: i32,
}

fn run_login(scratch: &ScratchVar, password: &str, code: Option<&str>) -> Login {
    let mut helper = PipeHelper::start(scratch, &["--stop-service"]);
    helper.send(json!({"username": USERNAME, "password": password})).unwrap();
    let mut result = helper.until(Some("code_required"));
    if result["phase"] == "code_required"
        && let Some(code) = code
    {
        helper.send(json!({"code": code})).unwrap();
        result = helper.until(None);
    }
    let (status, stderr) = helper.close();
    assert_eq!(stderr, "");
    let everything = helper.lines.join("\n");
    assert!(!everything.contains(password)); // neither the password nor the account name comes back
    assert!(!everything.contains(USERNAME));
    assert!(!scratch.data.join(".login-args").exists());
    Login { helper, result, status }
}

/// A clock the test moves forward.
#[derive(Default)]
struct MovedClock {
    ahead: Mutex<Duration>,
}

impl MovedClock {
    fn advance(&self, seconds: u64) {
        *self.ahead.lock().unwrap() += Duration::from_secs(seconds);
    }
}

impl Clock for MovedClock {
    fn monotonic(&self) -> Duration {
        SystemClock.monotonic() + *self.ahead.lock().unwrap()
    }

    fn now(&self) -> SystemTime {
        SystemClock.now()
    }
}

/// A clock on which time passes `factor` times as fast.
struct FastClock {
    factor: u32,
}

impl Clock for FastClock {
    fn monotonic(&self) -> Duration {
        SystemClock.monotonic() * self.factor
    }

    fn now(&self) -> SystemTime {
        SystemClock.now()
    }
}

/// Starts of the launcher a test did not let happen: (program, argument list).
type Attempts = Vec<(PathBuf, Vec<String>)>;

/// The system, with answers a test puts in its place.
#[derive(Default)]
struct Scripted {
    /// What a process runs, by pid: a program, or the error number of the failed look-up.
    programs: Mutex<HashMap<i32, Result<String, i32>>>,
    cmdlines: Mutex<HashMap<i32, Vec<String>>>,
    /// The settings under /proc/sys, when the test decides them.
    sysctls: Mutex<Option<HashMap<String, i64>>>,
    /// When set: nothing is started, every attempt is noted here.
    refused: Mutex<Option<Attempts>>,
}

impl System for Scripted {
    fn proc_start_time(&self, pid: i32) -> Option<u64> {
        Host.proc_start_time(pid)
    }

    fn proc_cmdline(&self, pid: i32) -> Vec<String> {
        self.cmdlines.lock().unwrap().get(&pid).cloned().unwrap_or_else(|| Host.proc_cmdline(pid))
    }

    fn proc_program(&self, pid: i32) -> io::Result<String> {
        match self.programs.lock().unwrap().get(&pid) {
            Some(Ok(program)) => Ok(program.clone()),
            Some(Err(errno)) => Err(io::Error::from_raw_os_error(*errno)),
            None => Host.proc_program(pid),
        }
    }

    fn proc_children(&self, pid: i32) -> Vec<i32> {
        Host.proc_children(pid)
    }

    fn pids(&self) -> Vec<i32> {
        Host.pids()
    }

    fn sysctl(&self, name: &str) -> Option<i64> {
        match &*self.sysctls.lock().unwrap() {
            Some(settings) => settings.get(name).copied(),
            None => Host.sysctl(name),
        }
    }

    fn spawn(&self, launch: Launch) -> io::Result<Child> {
        match &mut *self.refused.lock().unwrap() {
            Some(attempts) => {
                attempts.push((launch.program.clone(), launch.argv.clone()));
                Err(io::Error::new(io::ErrorKind::NotFound, "not started in this test"))
            }
            None => Host.spawn(launch),
        }
    }
}

// ----------------------------------------------------------------------------- PipeLoginTest

mod pipe_login_test {
    use super::*;

    #[test]
    fn sign_in_with_code() {
        let scratch = ScratchVar::new();
        scratch.earlier_sign_in();
        let login = run_login(&scratch, "pw-2fa", Some("123456"));
        assert_eq!((&login.result, login.status), (&json!({"event": "result", "ok": true}), 0));
        assert_eq!(login.helper.phases(), ["preparing", "signing_in", "code_required", "verifying", "finishing"]);
        assert!(scratch.account_is(&[
            ("DEV_TOKEN", "new-DEV_TOKEN"),
            ("MUSIC_TOKEN", "new-MUSIC_TOKEN"),
            ("STOREFRONT_ID", "new-STOREFRONT_ID")
        ]));
        let report = scratch.report();
        assert_eq!(report["argv"], json!(["--base-dir", "/data"])); // nothing secret on the command line
        assert_eq!(
            (&report["args_lines"], &report["args_mode"], &report["args_env"]),
            (&json!(5), &json!("0o600"), &json!("/data/.login-args"))
        );
        assert_eq!(report["tty"], json!([true, true, true])); // the wrapper's terminal prompt, no code file
        assert_eq!(report["codes"], json!(["123456"]));
        assert_eq!(report["kept"], json!(["MUSIC_TOKEN.before-login", "STOREFRONT_ID.before-login"]));
        assert_eq!(report["args_at_code"], false); // removed as soon as the wrapper has read it
        for entry in fs::read_dir(&scratch.data).unwrap().flatten() {
            assert_eq!(mode(&entry.path()) & 0o077, 0, "{:?}", entry.file_name());
        }
        // how the launcher is started: its own session, umask 077, an environment of its own
        assert_eq!((&report["own_session"], &report["umask"]), (&json!(true), &json!("0o77")));
        assert_eq!(report["env"], json!(["HOME", "LANG", "LITE_ARGS_FILE", "LITE_DNS_SERVERS", "PATH"]));
        // the event lines are Python's: a space after each separator
        assert_eq!(login.helper.lines[0], r#"{"event": "phase", "phase": "preparing"}"#);
        assert_eq!(login.helper.lines.last().unwrap(), r#"{"event": "result", "ok": true}"#);
    }

    #[test]
    fn sign_in_without_code() {
        let scratch = ScratchVar::new();
        let login = run_login(&scratch, "pw-plain", None);
        assert_eq!((&login.result["ok"], login.status), (&json!(true), 0));
        assert_eq!(login.helper.phases(), ["preparing", "signing_in", "finishing"]);
    }

    #[test]
    fn prompt_followed_by_another_log_line() {
        let scratch = ScratchVar::new();
        let login = run_login(&scratch, "pw-2fa-interleaved", Some("123456"));
        assert_eq!(login.result["ok"], true);
        assert!(login.helper.phases().contains(&"code_required"));
    }

    #[test]
    fn refused_code_ends_the_attempt_and_keeps_the_earlier_sign_in() {
        let scratch = ScratchVar::new();
        scratch.earlier_sign_in();
        let login = run_login(&scratch, "pw-2fa", Some("000000"));
        assert_eq!((&login.result["ok"], &login.result["kind"], login.status), (&json!(false), &json!("code"), 1));
        assert_eq!(scratch.report()["codes"], json!(["000000"])); // one code, never a second answer
        assert!(scratch.old_account());
    }

    #[test]
    fn second_password_request_ends_the_attempt() {
        let scratch = ScratchVar::new();
        scratch.earlier_sign_in();
        let started = Instant::now();
        let login = run_login(&scratch, "pw-wrong", None);
        assert_eq!(
            (&login.result["ok"], &login.result["kind"], login.status),
            (&json!(false), &json!("credentials"), 1)
        );
        assert!(started.elapsed() < Duration::from_secs(20)); // the wrapper was stopped, not waited for
        assert!(login.result["error"].as_str().unwrap().contains("locks"));
        assert_eq!(scratch.account()["MUSIC_TOKEN"], "old-MUSIC_TOKEN");
    }

    #[test]
    fn apple_message_is_passed_on_without_the_account_name() {
        let scratch = ScratchVar::new();
        let login = run_login(&scratch, "pw-apple", None);
        assert_eq!((&login.result["ok"], &login.result["kind"]), (&json!(false), &json!("apple")));
        assert_eq!(login.result["error"], "The Apple ID your Apple ID cannot sign in [31m right now.");
        assert!(login.result["steps"].as_array().unwrap().contains(&json!("auth_error")));
    }

    #[test]
    fn token_request_failure_after_an_accepted_sign_in() {
        let scratch = ScratchVar::new();
        scratch.earlier_sign_in();
        let login = run_login(&scratch, "pw-token-fail", None);
        assert_eq!((&login.result["ok"], &login.result["kind"]), (&json!(false), &json!("failed")));
        assert!(login.result["error"].as_str().unwrap().contains("token request failed"));
        assert_eq!(scratch.account()["MUSIC_TOKEN"], "old-MUSIC_TOKEN");
    }

    #[test]
    fn start_check() {
        let scratch = ScratchVar::new();
        let done = scratch.run(&["selftest"]);
        assert_eq!(
            (done.code, done.stdout.as_str(), done.stderr.as_str()),
            (0, "ok: the wrapper starts on this system\n", "")
        );
        for (printed, reason) in [
            ("unshare: Operation not permitted\n", "private namespace it runs in (unshare: Operation not permitted)"),
            ("uid_map: Permission denied\n", "private namespace it runs in (uid_map: Permission denied)"),
            ("mount proc failed: Operation not permitted\n", "private namespace"),
            ("mkdir ./rootfs/dev failed: Read-only file system\n", "could not prepare its folder"),
            ("execve: Permission denied\n", "mounted noexec"),
            ("CANNOT LINK EXECUTABLE \"/system/bin/lite\": library \"libx.so\" not found\n", "could not be loaded"),
            (
                "CANNOT LINK EXECUTABLE: could not load library \"libcurl.so.4\" needed by \"wrapper-lite-rootless\"; \
             caused by library \"libcurl.so.4\" not found\n",
                "built against this system's libcurl.so.4 instead of the library that comes with it, so it cannot be \
             loaded; `./setup.sh` builds it again (CANNOT LINK EXECUTABLE",
            ),
            ("something else\nSegmentation fault\n", "it stopped with: Segmentation fault"),
            ("", "it ended without printing anything"),
        ] {
            fs::write(scratch.runtime("selftest-fail"), printed).unwrap();
            let done = scratch.run(&["selftest"]);
            assert_eq!((done.code, done.stdout.as_str()), (1, ""), "{printed:?}");
            assert!(done.stderr.contains("cannot start on this computer"), "{printed:?}: {}", done.stderr);
            assert!(done.stderr.contains(reason), "{printed:?}: {}", done.stderr);
        }
        assert!(!scratch.runtime("stub-report.json").exists()); // no sign-in ran for any of this
    }

    #[test]
    fn a_system_that_cannot_start_the_wrapper_is_named_before_the_password_is_used() {
        let scratch = ScratchVar::new();
        scratch.earlier_sign_in();
        fs::write(scratch.runtime("selftest-fail"), "uid_map: Operation not permitted\n").unwrap();
        let mut helper = PipeHelper::start(&scratch, &["--stop-service"]);
        let _ = helper.send(json!({"username": USERNAME, "password": "pw-plain"})); // the helper may have answered already
        let result = helper.until(None);
        let (status, stderr) = helper.close();
        assert_eq!(
            (&result["ok"], &result["kind"], status, stderr.as_str()),
            (&json!(false), &json!("unavailable"), 1, "")
        );
        let error = result["error"].as_str().unwrap();
        assert!(error.contains("cannot start on this computer"));
        assert!(error.contains("(uid_map: Operation not permitted)"));
        assert!(helper.phases().is_empty()); // refused before anything is asked or written
        assert!(result.get("log").is_none());
        assert!(!scratch.runtime("stub-report.json").exists());
        assert!(!scratch.data.join(".login-args").exists());
        assert!(scratch.old_account());
        let done = scratch.run_with(&["login"], closed_pipe(), |_| {});
        assert_eq!(done.code, 1); // the terminal login refuses for the same reason
        assert!(done.stderr.contains("uid_map: Operation not permitted"));
    }

    #[test]
    fn a_wrapper_that_stops_before_it_starts_the_sign_in() {
        let scratch = ScratchVar::new();
        scratch.earlier_sign_in();
        fs::write(scratch.runtime("start-fail"), "execve: Permission denied\n").unwrap();
        let login = run_login(&scratch, "pw-plain", None);
        assert_eq!(
            (&login.result["ok"], &login.result["kind"], login.status),
            (&json!(false), &json!("unavailable"), 1)
        );
        let error = login.result["error"].as_str().unwrap();
        assert!(error.contains("did not start: this system refused to run its program (execve: Permission denied)"));
        assert_eq!(scratch.account()["MUSIC_TOKEN"], "old-MUSIC_TOKEN");
    }

    #[test]
    fn an_ending_without_a_reason_is_named_and_logged() {
        let scratch = ScratchVar::new();
        scratch.earlier_sign_in();
        let login = run_login(&scratch, "pw-no-reason", None);
        assert_eq!((&login.result["ok"], &login.result["kind"], login.status), (&json!(false), &json!("apple"), 1));
        let error = login.result["error"].as_str().unwrap();
        assert!(error.contains("without giving a reason (response type 3)"));
        assert!(error.ends_with(&format!("(response type 3). Details: {}", scratch.log_shown())), "{error}");
        assert_eq!(login.result["log"], scratch.log_file().display().to_string());
        assert_eq!(mode(&scratch.log_file()), 0o600);
        let log = fs::read_to_string(scratch.log_file()).unwrap();
        assert!(log.contains("result: apple: Apple ended the sign-in without giving a reason (response type 3)"));
        assert!(log.contains("steps: signing_in, password_request, auth_failed, login_failed"));
        assert!(log.contains("launcher: exit status 0"));
        assert!(log.contains("[INFO] auth failed: response type 3"));
        assert!(log.contains("Enter the password for <apple id>, 2FA: false}"));
        assert!(!log.contains(USERNAME));
        assert!(!log.contains("pw-no-reason"));
        assert_eq!(scratch.account()["MUSIC_TOKEN"], "old-MUSIC_TOKEN");
        // the form of the log: its head, then the lines
        let lines: Vec<&str> = log.lines().collect();
        assert!(lines[0].starts_with("am-ui sign-in log: what Apple's sign-in component printed"));
        assert!(
            lines[3].starts_with("when: 20")
                && lines[3].ends_with('Z')
                && lines[3].len() == "when: 2026-01-01T00:00:00Z".len()
        );
        assert_eq!(lines[7], format!("--- output ({} lines) ---", lines.len() - 8));
    }

    #[test]
    fn apples_dialog_is_passed_on_without_the_account_name() {
        let scratch = ScratchVar::new();
        let login = run_login(&scratch, "pw-dialog", None);
        assert_eq!((&login.result["ok"], &login.result["kind"]), (&json!(false), &json!("apple")));
        let error = login.result["error"].as_str().unwrap();
        let said = "Apple answered: Verification Failed: The account your Apple ID needs attention. Details: var/";
        assert!(error.starts_with(said), "{error}");
        // what Apple offered is in the log
        assert!(fs::read_to_string(scratch.log_file()).unwrap().contains("button 0x7f3ac0: OK"));
    }

    #[test]
    fn endings_without_an_answer() {
        let scratch = ScratchVar::new();
        for (password, said, steps) in [
            ("pw-no-answer", "without an answer from Apple", "login_failed"),
            ("pw-crash", "stopped unexpectedly while signing in", "password_request"),
        ] {
            let login = run_login(&scratch, password, None);
            assert_eq!((&login.result["ok"], &login.result["kind"]), (&json!(false), &json!("failed")), "{password}");
            let error = login.result["error"].as_str().unwrap();
            assert!(error.contains(said), "{password}: {error}");
            assert_eq!(login.result["steps"].as_array().unwrap().last().unwrap(), steps, "{password}");
            assert!(error.ends_with(&format!(". Details: {}", scratch.log_shown())), "{password}: {error}");
        }
    }

    #[test]
    fn the_log_leaves_out_what_was_typed_and_what_names_the_account() {
        let scratch = ScratchVar::new();
        let login = run_login(&scratch, "pw-echo", Some("654321"));
        assert_eq!((&login.result["ok"], &login.result["kind"]), (&json!(false), &json!("apple")));
        let log = fs::read_to_string(scratch.log_file()).unwrap();
        for secret in ["pw-echo", "654321", USERNAME, "other@example.org", "eyJ", "xxxx", "1234567890"] {
            assert!(!log.contains(secret), "{secret}");
        }
        assert_eq!(log.matches("<line left out: it contained something typed into the sign-in>").count(), 2);
        assert!(log.contains("token <left out> for <apple id> account <number>"));
        assert!(log.contains("auth failed: response type 5"));
    }

    #[test]
    fn a_later_success_removes_the_log() {
        let scratch = ScratchVar::new();
        run_login(&scratch, "pw-no-answer", None);
        assert!(scratch.log_file().exists());
        let login = run_login(&scratch, "pw-plain", None);
        assert_eq!(login.result["ok"], true);
        assert!(!scratch.log_file().exists());
    }

    #[test]
    fn code_file_fallback_of_the_wrapper_is_not_used() {
        let scratch = ScratchVar::new();
        let login = run_login(&scratch, "pw-code-file", None);
        assert_eq!((&login.result["ok"], &login.result["kind"]), (&json!(false), &json!("failed")));
    }

    #[test]
    fn cancel_and_closed_input() {
        let scratch = ScratchVar::new();
        scratch.earlier_sign_in();
        // "terminate" is a signal to the helper, so there it is a program of its own; "interrupt"
        // (not in the Python test) is the same request to a helper inside this process
        for how in ["cancel", "close", "terminate", "interrupt"] {
            let mut helper = match how {
                "terminate" => PipeHelper::start_process(&scratch, &[]),
                _ => PipeHelper::start(&scratch, &[]),
            };
            helper.send(json!({"username": USERNAME, "password": "pw-hang"})).unwrap();
            helper.until(Some("signing_in"));
            match (how, &helper.running, &helper.interrupt) {
                ("cancel", _, _) => helper.send(json!({"cancel": true})).unwrap(),
                ("close", _, _) => drop(helper.stdin.take()),
                // SAFETY: SIGTERM to the helper this test started, which has not been waited for
                ("terminate", Running::Process(child), _) => {
                    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0)
                }
                ("interrupt", _, Some(interrupt)) => interrupt.raise(libc::SIGHUP),
                _ => unreachable!(),
            }
            let result = helper.until(None);
            let (status, _) = helper.close();
            assert_eq!((&result["ok"], &result["kind"], status), (&json!(false), &json!("cancelled"), 1), "{how}");
            assert!(scratch.old_account(), "{how}");
            assert!(!scratch.log_file().exists(), "{how}"); // a cancelled sign-in has nothing to explain
            assert!(!scratch.data.join(".login-args").exists(), "{how}");
            // the wrapper was stopped with it
            let wrapper = scratch.report()["pid"].as_i64().unwrap();
            wait_for("the stand-in wrapper to end", || !Path::new(&format!("/proc/{wrapper}")).exists());
        }
    }

    #[test]
    fn input_checks_come_before_anything_is_touched() {
        let scratch = ScratchVar::new();
        scratch.earlier_sign_in();
        for (message, kind) in [
            (json!({"username": USERNAME}), "input"),
            (json!({"username": "a:b@example.com", "password": "x"}), "input"),
            (json!({"username": USERNAME, "password": "x".repeat(58)}), "input"),
            (json!({"cancel": true}), "cancelled"),
        ] {
            let mut helper = PipeHelper::start(&scratch, &[]);
            helper.send(message.clone()).unwrap();
            let result = helper.until(None);
            helper.close();
            assert_eq!((&result["ok"], &result["kind"]), (&json!(false), &json!(kind)), "{message}");
            assert!(!scratch.runtime("stub-report.json").exists()); // the wrapper never ran
            assert_eq!(scratch.account()["MUSIC_TOKEN"], "old-MUSIC_TOKEN");
        }
    }

    #[test]
    fn invalid_code_lines_are_refused_without_reaching_the_wrapper() {
        let scratch = ScratchVar::new();
        let mut helper = PipeHelper::start(&scratch, &[]);
        helper.send(json!({"username": USERNAME, "password": "pw-2fa"})).unwrap();
        helper.until(Some("code_required"));
        helper.send(json!({"code": "12345"})).unwrap();
        let notice = helper.read_event(Duration::from_secs(20));
        assert_eq!(notice, Some(json!({"event": "notice", "error": "The verification code is six digits"})));
        helper.send(json!({"code": "123456"})).unwrap();
        let result = helper.until(None);
        helper.close();
        assert_eq!(result["ok"], true);
        assert_eq!(scratch.report()["codes"], json!(["123456"]));
    }

    #[test]
    fn a_wrapper_this_helper_did_not_start_is_left_alone() {
        let scratch = ScratchVar::new();
        let _server = scratch.status_server(json!({"code": 0, "data": {"regions": ["test"]}}));
        scratch.earlier_sign_in();
        let mut helper = PipeHelper::start(&scratch, &["--stop-service"]);
        helper.send(json!({"username": USERNAME, "password": "pw-plain"})).unwrap();
        let result = helper.until(None);
        helper.close();
        assert_eq!((&result["ok"], &result["kind"]), (&json!(false), &json!("busy")));
        assert!(!scratch.runtime("stub-report.json").exists());
        assert_eq!(scratch.account()["MUSIC_TOKEN"], "old-MUSIC_TOKEN");
    }

    #[test]
    fn leftovers_of_an_interrupted_sign_in_are_cleared() {
        let scratch = ScratchVar::new();
        fs::write(scratch.data.join(".login-args"), "stale\n").unwrap();
        fs::write(scratch.data.join("MUSIC_TOKEN.before-login"), "old-MUSIC_TOKEN\n").unwrap();
        let login = run_login(&scratch, "pw-apple", None);
        assert_eq!(login.result["ok"], false);
        assert!(scratch.account_is(&[("MUSIC_TOKEN", "old-MUSIC_TOKEN")])); // the kept file is back in place
    }

    #[test]
    fn the_service_does_not_start_during_a_sign_in() {
        let scratch = ScratchVar::new();
        let mut helper = PipeHelper::start(&scratch, &["--stop-service"]);
        helper.send(json!({"username": USERNAME, "password": "pw-2fa"})).unwrap();
        assert_eq!(helper.until(Some("code_required"))["phase"], "code_required");
        let start = scratch.run(&["start"]);
        assert_ne!(start.code, 0);
        assert!(start.stderr.contains("another lossless helper command is running")); // the sign-in holds the lock
        assert!(!scratch.var.join("wrapper/run/service.pid.json").exists());
        helper.send(json!({"cancel": true})).unwrap();
        assert_eq!(helper.until(None)["kind"], "cancelled");
        helper.close();
    }

    #[test]
    fn a_wrapper_built_before_the_one_answer_patch_does_not_sign_in() {
        let scratch = ScratchVar::new();
        scratch.earlier_sign_in();
        fs::write(scratch.var.join("wrapper/rootfs/system/bin/lite"), "stand-in\n").unwrap(); // staged without patches/0004
        let mut helper = PipeHelper::start(&scratch, &["--stop-service"]);
        let result = helper.until(None);
        let (status, stderr) = helper.close();
        assert_eq!(
            (&result["ok"], &result["kind"], status, stderr.as_str()),
            (&json!(false), &json!("unavailable"), 1, "")
        );
        assert!(result["error"].as_str().unwrap().contains("./setup.sh"));
        assert_eq!(helper.events.len(), 1); // refused before anything is asked
        assert!(!scratch.runtime("stub-report.json").exists()); // the wrapper never ran
        assert!(scratch.old_account());
        let done = scratch.run_with(&["login"], closed_pipe(), |_| {});
        assert_eq!(done.code, 1); // the terminal login refuses for the same reason
        assert!(done.stderr.contains("./setup.sh"));
    }

    #[test]
    fn terminal_login_refuses_pipes() {
        let scratch = ScratchVar::new();
        let done = scratch.run_with(&["login"], closed_pipe(), |_| {});
        assert_eq!(done.code, 1);
        assert!(done.stderr.contains("your own terminal"));
    }
}

// ----------------------------------------------------------------------------- LauncherStartTest

/// How the helper starts the launcher and recognises a running one. The launcher hands its own
/// argv[0] on to the wrapper, and the real wrapper ends without a word when that is 128 characters
/// or longer: started by its full path, an am-ui folder deep in the file system would break
/// lossless and the sign-in.
mod launcher_start_test {
    use super::*;

    #[test]
    fn the_launcher_is_started_by_a_short_name() {
        let scratch = ScratchVar::new();
        let launcher = scratch.var.join("build/lossless/bin/wrapper-lite-rootless");
        let system = Arc::new(Scripted { refused: Mutex::new(Some(Vec::new())), ..Scripted::default() });
        let mut context = scratch.context(null(), null(), null());
        context.system = system.clone();
        let helper = wrapper::Helper::new(&context);
        assert_eq!(
            helper.launcher_selftest().unwrap(),
            (false, "its launcher could not be run (not started in this test)".to_string())
        );
        for foreground in [false, true] {
            assert!(helper.spawn_launcher(&["--port", "1"], &[], foreground, None).is_err());
        }
        let started = system.refused.lock().unwrap().take().unwrap();
        let argument_lists: Vec<&[String]> = started.iter().map(|(_, argv)| argv.as_slice()).collect();
        assert_eq!(
            argument_lists,
            [
                &strings(&["wrapper-lite-rootless", "--base-dir", "/data", "--help"])[..],
                &strings(&["wrapper-lite-rootless", "--base-dir", "/data", "--port", "1"]),
                &strings(&["wrapper-lite-rootless", "--base-dir", "/data", "--port", "1"]),
            ]
        );
        for (program, _) in &started {
            assert_eq!(program, &launcher);
        }
        // nowhere started by its full path: one place starts programs, under the first argument's name
        let source = include_str!("../src/wrapper.rs");
        assert_eq!(source.matches("Command::new(").count(), 1);
        assert!(source.contains("command.arg0(name).args(arguments);"));
    }

    #[test]
    fn running_launchers_and_wrappers_are_recognised_by_their_program() {
        let scratch = ScratchVar::new();
        let launcher = scratch.var.join("build/lossless/bin/wrapper-lite-rootless").display().to_string();
        let me = std::process::id() as i32;
        let system = Arc::new(Scripted::default());
        let mut context = scratch.context(null(), null(), null());
        context.system = system.clone();
        assert!(!wrapper::Helper::new(&context).is_wrapper_process(me));
        let wrapper_program = scratch.var.join("wrapper/rootfs/system/bin/lite").display().to_string();
        for program in [
            launcher.clone(),
            format!("{launcher} (deleted)"), // replaced by a newer build while it runs
            wrapper_program,                 // the wrapper itself: it outlives a launcher that was killed
        ] {
            system.programs.lock().unwrap().insert(me, Ok(program.clone()));
            assert!(wrapper::Helper::new(&context).is_wrapper_process(me), "{program}");
        }
        system.programs.lock().unwrap().insert(me, Err(libc::EACCES));
        assert!(!wrapper::Helper::new(&context).is_wrapper_process(me));
        system.cmdlines.lock().unwrap().insert(me, strings(&[&launcher, "--base-dir", "/data"]));
        assert!(wrapper::Helper::new(&context).is_wrapper_process(me)); // started by its full path, as earlier versions did
        system.programs.lock().unwrap().clear();
        system.cmdlines.lock().unwrap().clear();
        context.paths.launcher = fs::read_link("/proc/self/exe").unwrap();
        let helper = wrapper::Helper::new(&context);
        assert!(helper.is_wrapper_process(me)); // read from /proc for real
        assert!(!helper.other_launchers().contains(&me)); // the asking process itself does not count
    }

    #[test]
    fn the_service_and_its_wrapper_are_not_taken_for_a_sign_in() {
        let scratch = ScratchVar::new();
        let mut child = Command::new("sleep").arg("60").stdin(Stdio::null()).spawn().unwrap();
        let child_pid = child.id() as i32;
        let me = std::process::id() as i32;
        let system = Arc::new(Scripted::default());
        let mut context = scratch.context(null(), null(), null());
        context.system = system.clone();
        // the launcher is the program the child runs
        wait_for("the child to run its program", || {
            fs::read_link(format!("/proc/{child_pid}/exe"))
                .is_ok_and(|program| program != fs::read_link("/proc/self/exe").unwrap())
        });
        context.paths.launcher = fs::read_link(format!("/proc/{child_pid}/exe")).unwrap();
        let launcher_name = context.paths.launcher.file_name().unwrap().to_string_lossy().into_owned();
        let pidfile = scratch.var.join("wrapper/run/service.pid.json");
        fs::create_dir_all(pidfile.parent().unwrap()).unwrap();
        {
            let helper = wrapper::Helper::new(&context);
            assert!(helper.other_launchers().contains(&child_pid)); // no service recorded: it counts
            // this process recorded as the service: the child is part of it
            fs::write(&pidfile, json!({"pid": me, "start_time": Host.proc_start_time(me)}).to_string()).unwrap();
            system.cmdlines.lock().unwrap().insert(me, vec![launcher_name]);
            assert!(helper.pidfile_process_alive(&helper.read_pidfile().unwrap()));
            assert!(!helper.other_launchers().contains(&child_pid));
            // the record of a process that is gone does not cover it
            fs::write(&pidfile, json!({"pid": me, "start_time": 1}).to_string()).unwrap();
            assert!(helper.other_launchers().contains(&child_pid));
        }
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

// ----------------------------------------------------------------------------- DoctorStartCheckTest

mod doctor_start_check_test {
    use super::*;

    /// What the doctor reads from `selftest`: nothing when it starts, else the last line the helper
    /// wrote to standard error, without its "error: ".
    #[test]
    fn the_doctor_names_a_wrapper_that_cannot_start() {
        let scratch = ScratchVar::new();
        let start_problem = || {
            let done = scratch.run(&["selftest"]);
            if done.code == 0 {
                return String::new();
            }
            let last = done.stderr.lines().map(str::trim).rfind(|line| !line.is_empty());
            last.map_or("the start check failed", |line| line.strip_prefix("error: ").unwrap_or(line)).to_string()
        };
        assert_eq!(start_problem(), "");
        fs::write(scratch.runtime("selftest-fail"), "unshare: Operation not permitted\n").unwrap();
        let problem = start_problem();
        let named = "Apple's component for the sign-in and for lossless cannot start on this computer: this system did not \
                 let it set up the private namespace";
        assert!(problem.starts_with(named), "{problem}");
        assert!(problem.contains("(unshare: Operation not permitted)"));
    }
}

// ----------------------------------------------------------------------------- BuildStateTest

/// `build-state`: what ./setup.sh asks before it builds the lossless route.
mod build_state_test {
    use super::*;

    #[test]
    fn states() {
        let scratch = ScratchVar::new();
        let (build, runtime) = (scratch.var.join("build/lossless"), scratch.var.join("wrapper"));
        assert_eq!(scratch.state(), "absent stopped"); // the downloader and the rip helper are not there
        for name in ["amdl", "amui-fairplay-rip"] {
            fs::write(build.join("bin").join(name), "stand-in\n").unwrap();
        }
        assert_eq!(scratch.state(), "stale stopped"); // no record of what it was built from
        let fingerprint = scratch.run(&["fingerprint"]).stdout.trim().to_string();
        assert!(
            fingerprint.len() == 64 && fingerprint.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        );
        fs::write(build.join("manifest.json"), json!({"source_fingerprint": "0".repeat(64)}).to_string()).unwrap();
        assert_eq!(scratch.state(), "stale stopped"); // built from other inputs
        fs::write(build.join("manifest.json"), json!({"source_fingerprint": fingerprint}).to_string()).unwrap();
        assert_eq!(scratch.state(), "unstaged stopped");
        for folder in [build.join("wrapper-src/rootfs/system/bin"), runtime.join("rootfs/system/bin")] {
            fs::write(folder.join("linker64"), "stand-in linker\n").unwrap();
        }
        fs::create_dir(runtime.join("amdl")).unwrap();
        fs::write(runtime.join("amdl/config.yaml"), "stand-in\n").unwrap();
        assert_eq!(scratch.state(), "current stopped");
        let staged = fs::read_to_string(runtime.join("rootfs/system/bin/lite")).unwrap();
        fs::write(runtime.join("rootfs/system/bin/lite"), "an older build\n").unwrap();
        assert_eq!(scratch.state(), "unstaged stopped"); // the runtime holds another build
        fs::write(runtime.join("rootfs/system/bin/lite"), staged).unwrap();

        let _server = scratch.status_server(json!({"code": 0, "data": {"regions": ["test"]}}));
        assert_eq!(scratch.state(), "current running");
        let stage = scratch.run(&["stage"]);
        assert_ne!(stage.code, 0); // the runtime is never restaged under a running service
        assert!(stage.stderr.contains("the service is running"));
    }
}

// ----------------------------------------------------------------------------- TerminalSignInTest

/// The sign-in in a terminal (a pseudo-terminal here), the helper's own prompts answered there:
/// what `./am-ui cli connect` runs on the native route.
mod terminal_sign_in_test {
    use super::*;

    /// Run `login --stop-service` on a pseudo-terminal; answers = (text to wait for, line to type).
    /// The helper has no controlling terminal here, so the hidden prompt reads this pseudo-terminal.
    fn run_in_terminal(scratch: &ScratchVar, answers: &[(&str, &str)]) -> (i32, String) {
        let (master, slave) = open_pty();
        let context = scratch.context(slave.try_clone().unwrap(), slave.try_clone().unwrap(), slave);
        assert!(context.stdin_is_terminal && context.stdout_is_terminal && context.controlling_terminal.is_none());
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let code = wrapper::run(&strings(&["login", "--stop-service"]), &context);
            drop(context);
            let _ = sender.send(code);
        });
        let mut master = File::from(master);
        let mut output = Vec::new();
        let mut answered = 0; // the next answer waits for text that comes after this point
        let mut pending = answers.to_vec();
        let mut code = None;
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if !wait_readable(master.as_fd(), Duration::from_millis(200)) {
                code = code.or_else(|| receiver.try_recv().ok());
                if code.is_some() {
                    break;
                }
                continue;
            }
            let mut chunk = [0u8; 4096];
            match master.read(&mut chunk) {
                Ok(count) if count > 0 => output.extend_from_slice(&chunk[..count]),
                _ => break, // the other end is closed
            }
            if let Some((wanted, line)) = pending.first()
                && contains(&output[answered..], wanted.as_bytes())
            {
                master.write_all(format!("{line}\n").as_bytes()).unwrap();
                answered = output.len();
                pending.remove(0);
            }
        }
        let code = code.or_else(|| receiver.recv_timeout(Duration::from_secs(30)).ok()).expect("the login ended");
        let output = String::from_utf8_lossy(&output).into_owned();
        assert!(pending.is_empty(), "{output}");
        (code, output)
    }

    #[test]
    fn sign_in_with_code() {
        let scratch = ScratchVar::new();
        scratch.earlier_sign_in();
        let answers = [("Apple account email:", USERNAME), ("(hidden):", "pw-2fa"), ("2FA code:", "123456")];
        let (code, output) = run_in_terminal(&scratch, &answers);
        assert_eq!(code, 0, "{output}");
        assert!(!output.contains("pw-2fa")); // the password is not echoed
        assert!(scratch.account_is(&[
            ("DEV_TOKEN", "new-DEV_TOKEN"),
            ("MUSIC_TOKEN", "new-MUSIC_TOKEN"),
            ("STOREFRONT_ID", "new-STOREFRONT_ID")
        ]));
        assert!(!output.contains("new-MUSIC_TOKEN"));
        assert!(output.contains("Login completed: fresh account token files exist in the private runtime"));
        // the launcher ran on the terminal itself, in the helper's session, with nothing secret in its arguments
        let report = scratch.report();
        assert_eq!((&report["tty"], &report["own_session"]), (&json!([true, true, true]), &json!(false)));
        assert_eq!((&report["argv"], &report["args_mode"]), (&json!(["--base-dir", "/data"]), &json!("0o600")));
        assert!(!scratch.data.join(".login-args").exists());
    }

    #[test]
    fn failed_attempt_keeps_the_earlier_sign_in() {
        let scratch = ScratchVar::new();
        scratch.earlier_sign_in();
        let (code, output) =
            run_in_terminal(&scratch, &[("Apple account email:", USERNAME), ("(hidden):", "pw-apple")]);
        assert_eq!(code, 1, "{output}");
        assert!(output.contains("error: login did not produce fresh account tokens"));
        assert_eq!(scratch.account()["MUSIC_TOKEN"], "old-MUSIC_TOKEN");
        assert!(!scratch.data.join(".login-args").exists());
    }

    /// The wrapper side of the Python test of this name (there `amui.session` refuses first): without
    /// a terminal nothing is asked and nothing is started.
    #[test]
    fn needs_a_terminal() {
        let scratch = ScratchVar::new();
        let done = scratch.run_with(&["login", "--stop-service"], null(), |_| {});
        assert_eq!(done.code, 1);
        assert_eq!(
            done.stderr,
            "error: run the login from your own terminal; never send passwords or codes through chat\n"
        );
        assert_eq!(done.stdout, "");
        assert!(!scratch.runtime("stub-report.json").exists()); // nothing was started
    }
}

// ----------------------------------------------------------------------------- beyond the Python tests

#[test]
fn a_process_that_has_ended_counts_as_gone() {
    let mut child = Command::new("true").stdin(Stdio::null()).spawn().unwrap();
    let pid = child.id() as i32;
    // ended, but not collected by this process yet: /proc still shows it
    wait_for("the child to end", || {
        fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| stat.contains(") Z "))
    });
    assert_eq!(Host.proc_start_time(pid), None);
    assert!(Host.proc_start_time(std::process::id() as i32).is_some());
    // so a pidfile that names it is stale, and nothing is signalled
    let scratch = ScratchVar::new();
    let pidfile = scratch.var.join("wrapper/run/service.pid.json");
    fs::create_dir_all(pidfile.parent().unwrap()).unwrap();
    fs::write(&pidfile, json!({"pid": pid, "start_time": 1}).to_string()).unwrap();
    let done = scratch.run(&["stop"]);
    assert_eq!(
        done.stdout,
        "{\n  \"running\": false,\n  \"stopped\": false,\n  \"note\": \"stale pidfile removed\"\n}\n"
    );
    child.wait().unwrap();
    assert_eq!(Host.proc_start_time(pid), None);
}

#[test]
fn the_fingerprint_is_the_script_s() {
    let scratch = ScratchVar::new();
    let done = scratch.run(&["fingerprint"]);
    assert_eq!((done.code, done.stdout.as_str(), done.stderr.as_str()), (0, &format!("{FINGERPRINT}\n")[..], ""));
    // every input counts
    fs::write(scratch.root.join("tools/lossless/patches/0003-third.patch"), "third\n").unwrap();
    assert_ne!(scratch.run(&["fingerprint"]).stdout.trim(), FINGERPRINT);
    fs::remove_file(scratch.root.join("tools/lossless/patches/0003-third.patch")).unwrap();
    fs::write(scratch.root.join("tools/lossless/patches/notes.txt"), "not a patch\n").unwrap();
    assert_eq!(scratch.run(&["fingerprint"]).stdout.trim(), FINGERPRINT);
    fs::remove_file(scratch.root.join("tools/lossless/build.sh")).unwrap();
    let done = scratch.run(&["fingerprint"]);
    assert_eq!((done.code, done.stdout.as_str()), (1, ""));
    assert!(done.stderr.starts_with("error: ") && done.stderr.ends_with("build.sh: No such file or directory\n"));
}

#[test]
fn paths_are_printed_for_people_and_for_the_shell() {
    let scratch = ScratchVar::new();
    let var = scratch.var.display().to_string();
    let done = scratch.run(&["paths", "--sh"]);
    let expected = [
        format!("export AMUI_LOSSLESS_ROOT=\"{}\"", scratch.root.display()),
        format!("export AMUI_LOSSLESS_BUILD=\"{var}/build/lossless\""),
        format!("export AMUI_LOSSLESS_RUNTIME=\"{var}/wrapper\""),
        format!("export AMUI_LOSSLESS_DATA=\"{var}/wrapper/rootfs/data\""),
        format!("export AMUI_LOSSLESS_LAUNCHER=\"{var}/build/lossless/bin/wrapper-lite-rootless\""),
        format!("export AMUI_LOSSLESS_PORT=\"{}\"", scratch.port),
        format!("export AMUI_LOSSLESS_STATUS_URL=\"http://127.0.0.1:{}/status\"", scratch.port),
        format!("export AMUI_LOSSLESS_SERVICE_LOG=\"{var}/wrapper/rootfs/data/service.log\""),
    ];
    assert_eq!((done.code, done.stderr.as_str()), (0, ""));
    assert_eq!(done.stdout.lines().collect::<Vec<_>>(), expected);
    let done = scratch.run(&["paths"]);
    assert_eq!((done.code, done.stderr.as_str()), (0, ""));
    assert!(done.stdout.starts_with(&format!("{{\n  \"AMUI_LOSSLESS_ROOT\": \"{}\",\n", scratch.root.display())));
    let values: Value = serde_json::from_str(&done.stdout).unwrap();
    assert_eq!(values.as_object().unwrap().len(), 8);
    assert_eq!(values["AMUI_LOSSLESS_PORT"], scratch.port.to_string());
    // the same lines from the helper as a program of its own
    let program = scratch.process(&["paths", "--sh"]).stdin(Stdio::null()).output().unwrap();
    assert_eq!(program.status.code(), Some(0));
    assert_eq!(String::from_utf8(program.stdout).unwrap().lines().collect::<Vec<_>>(), expected);
    assert_eq!(program.stderr, b"");
}

#[test]
fn usage_errors_end_with_status_2_and_help_with_0() {
    let scratch = ScratchVar::new();
    let done = scratch.run(&[]);
    assert_eq!((done.code, done.stdout.as_str()), (2, ""));
    assert_eq!(
        done.stderr,
        "usage: wrapperctl [-h]\n                  {paths,fingerprint,build-state,stage,start,stop,status,selftest,login} \
         ...\nwrapperctl: error: the following arguments are required: command\n"
    );
    let done = scratch.run(&["login", "--pipe", "--nonsense"]);
    assert_eq!((done.code, done.stdout.as_str()), (2, "")); // no event line: the window sees the exit status
    assert!(done.stderr.ends_with("wrapperctl: error: unrecognized arguments: --nonsense\n"));
    let done = scratch.run(&["stop", "--help"]);
    assert_eq!((done.code, done.stderr.as_str()), (0, ""));
    assert_eq!(done.stdout, "usage: wrapperctl stop [-h]\n\noptions:\n  -h, --help  show this help message and exit\n");
    assert!(!scratch.var.join("wrapper/run").exists()); // none of this touched the runtime
    let program = scratch.process(&["nonsense"]).stdin(Stdio::null()).output().unwrap();
    assert_eq!(program.status.code(), Some(2));
    assert!(String::from_utf8(program.stderr).unwrap().contains("invalid choice: 'nonsense' (choose from 'paths', "));
}

#[test]
fn login_explains_itself() {
    let scratch = ScratchVar::new();
    let done = scratch.run(&["login", "--explain-only"]);
    assert_eq!((done.code, done.stderr.as_str()), (0, ""));
    assert!(
        done.stdout
            .starts_with("Apple's native sign-in for am-ui (the sign-in that also makes lossless available)\n---")
    );
    assert!(done.stdout.contains(&format!("\nWhere state goes: {}\n  mpl_db/", scratch.data.display())));
    assert!(done.stdout.ends_with("sign-in on this machine is kept.\n\n"));
    assert!(!scratch.runtime("stub-report.json").exists());
}

#[test]
fn nothing_built_is_said_plainly() {
    let scratch = ScratchVar::new();
    fs::remove_file(scratch.var.join("build/lossless/bin/wrapper-lite-rootless")).unwrap();
    let build = scratch.root.join("tools/lossless/build.sh").display().to_string();
    for args in [&["selftest"][..], &["stage"], &["start"], &["login"], &["login", "--explain-only"]] {
        let done = scratch.run(args);
        assert_eq!((done.code, done.stdout.as_str()), (1, ""), "{args:?}");
        assert_eq!(done.stderr, format!("error: wrapper not built; run {build} first\n"), "{args:?}");
    }
    let done = scratch.run(&["login", "--pipe"]);
    let line = format!(
        "{{\"event\": \"result\", \"ok\": false, \"kind\": \"unavailable\", \"error\": \"wrapper not built; run {build} \
         first\", \"steps\": []}}\n"
    );
    assert_eq!((done.code, done.stdout.as_str(), done.stderr.as_str()), (1, &line[..], ""));
    let status: Value = serde_json::from_str(&scratch.run(&["status"]).stdout).unwrap();
    assert_eq!(
        (&status["built"], &status["runtime_staged"], &status["running"]),
        (&json!(false), &json!(true), &json!(false))
    );
    // staged from a build that is gone
    fs::remove_file(scratch.var.join("wrapper/rootfs/system/etc/workspace-ca-bundle.pem")).unwrap();
    install_script(&scratch.var.join("build/lossless/bin/wrapper-lite-rootless"), "#!/bin/sh\nexit 0\n");
    let done = scratch.run(&["start"]);
    let runtime = scratch.var.join("wrapper").display().to_string();
    assert_eq!(done.stderr, format!("error: runtime not staged under {runtime}; run {build} first\n"));
}

/// A build tree as build.sh leaves it, with libraries whose hashes are pinned, and a trust store.
fn build_tree(scratch: &ScratchVar) -> PathBuf {
    let build = scratch.var.join("build/lossless");
    let system = build.join("wrapper-src/rootfs/system");
    fs::create_dir_all(system.join("lib64")).unwrap();
    let libraries: [(&str, Vec<u8>); 5] = [
        ("libA.so", b"library A\n".to_vec()),
        ("libB.so", b"library B\n".repeat(1000)),
        ("libc++_shared.so", b"c++\n".to_vec()),
        ("notes.txt", b"not a library\n".to_vec()),
        ("libold.so.1", b"old\n".to_vec()),
    ];
    let mut pins = String::from("# stand-in pins\n");
    for (name, content) in &libraries {
        fs::write(system.join("lib64").join(name), content).unwrap();
        if matches!(*name, "libA.so" | "libB.so") {
            let hash: String = Sha256::digest(content).iter().map(|byte| format!("{byte:02x}")).collect();
            pins.push_str(&format!("{hash}  {name}\n"));
        }
    }
    pins.push_str("short  libC.so\n\n");
    fs::write(scratch.root.join("tools/lossless/fairplay-libs.sha256"), pins).unwrap();
    fs::write(system.join("bin/linker64"), "stand-in linker\n").unwrap();
    for name in ["amdl", "amui-fairplay-rip"] {
        fs::write(build.join("bin").join(name), "stand-in\n").unwrap();
    }
    fs::write(
        build.join("manifest.json"),
        "{\n  \"built_at\": \"2026-01-01T00:00:00+0000\",\n  \"inputs\": \"fetch\"\n}\n",
    )
    .unwrap();
    let bundle = scratch.root.join("stand-in-ca.pem");
    let certificate = "-----BEGIN CERTIFICATE-----\nstand-in\n-----END CERTIFICATE-----\n";
    fs::write(&bundle, certificate.repeat(12)).unwrap();
    bundle
}

#[test]
fn stage_copies_the_build_into_the_runtime() {
    let scratch = ScratchVar::new();
    let bundle = build_tree(&scratch);
    let bundle_named = bundle.display().to_string();
    let stage = |bundle: &str| {
        scratch.run_with(&["stage"], null(), |context| {
            context.env.insert("LOSSLESS_CA_BUNDLE".to_string(), bundle.to_string());
        })
    };
    let done = stage(&bundle_named);
    assert_eq!((done.code, done.stderr.as_str()), (0, ""), "{}", done.stderr);
    let report: Value = serde_json::from_str(&done.stdout).unwrap();
    let runtime = scratch.var.join("wrapper");
    assert_eq!(
        report,
        json!({
            "runtime": runtime.display().to_string(),
            "files_copied": 5, // three libraries, the linker and the trust store; the wrapper was there already
            "libraries": 3,
            "pinned_verified": 2,
            "ca_bundle": bundle_named,
            "account_data_dir": scratch.data.display().to_string(),
            "account_state": {
                "music_token_file": false,
                "storefront_file": false,
                "dev_token_file": false,
                "account_database": false,
                "login_in_progress": false,
            },
        })
    );
    assert!(done.stdout.starts_with("{\n  \"runtime\": \"") && done.stdout.ends_with("\n  }\n}\n"));
    let system = runtime.join("rootfs/system");
    for (path, expected) in [
        ("lib64/libA.so", 0o444),
        ("lib64/libB.so", 0o444),
        ("lib64/libc++_shared.so", 0o444),
        ("bin/linker64", 0o755),
        ("bin/lite", 0o755),
        ("etc/workspace-ca-bundle.pem", 0o444),
        ("lib64", 0o700),
    ] {
        assert_eq!(mode(&system.join(path)), expected, "{path}");
    }
    for folder in [&scratch.var, &runtime, &runtime.join("run"), &runtime.join("logs"), &scratch.data] {
        assert_eq!(mode(folder), 0o700, "{folder:?}");
    }
    assert!(!system.join("lib64/notes.txt").exists() && !system.join("lib64/libold.so.1").exists());
    assert_eq!(fs::read(system.join("lib64/libB.so")).unwrap(), b"library B\n".repeat(1000));
    assert_eq!(fs::read(system.join("etc/workspace-ca-bundle.pem")).unwrap(), fs::read(&bundle).unwrap());
    assert_eq!(mode(&runtime.join("manifest.json")), 0o600);
    let text = fs::read_to_string(runtime.join("manifest.json")).unwrap();
    assert!(text.starts_with("{\n  \"staged_at\": \"20") && text.ends_with("}\n"));
    let manifest: Value = serde_json::from_str(&text).unwrap();
    let keys: Vec<&String> = manifest.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["staged_at", "libraries", "binaries", "ca_bundle", "build"]);
    let hash_of =
        |content: &[u8]| -> String { Sha256::digest(content).iter().map(|byte| format!("{byte:02x}")).collect() };
    assert_eq!(manifest["libraries"]["libA.so"], json!({"sha256": hash_of(b"library A\n"), "pinned": true}));
    assert_eq!(manifest["libraries"]["libc++_shared.so"]["pinned"], false);
    let binaries: Vec<&String> = manifest["binaries"].as_object().unwrap().keys().collect();
    assert_eq!(binaries, ["linker64", "lite", "wrapper-lite-rootless"]);
    assert_eq!(manifest["binaries"]["linker64"], hash_of(b"stand-in linker\n"));
    assert_eq!(manifest["ca_bundle"]["certificates"], 12);
    assert_eq!(manifest["ca_bundle"]["source"], bundle_named);
    assert_eq!(manifest["build"], json!({"built_at": "2026-01-01T00:00:00+0000", "inputs": "fetch"}));

    // a second time nothing is copied
    let again: Value = serde_json::from_str(&stage(&bundle_named).stdout).unwrap();
    assert_eq!(again["files_copied"], 0);

    // what is refused, and nothing of it reaches the runtime
    let library = scratch.var.join("build/lossless/wrapper-src/rootfs/system/lib64/libA.so");
    fs::write(&library, "tampered\n").unwrap();
    let done = stage(&bundle_named);
    let refusal =
        format!("error: libA.so does not match its pinned SHA-256 ({}); refusing to stage\n", hash_of(b"tampered\n"));
    assert_eq!((done.code, done.stdout.as_str(), done.stderr.as_str()), (1, "", &refusal[..]));
    assert_eq!(fs::read(system.join("lib64/libA.so")).unwrap(), b"library A\n");
    fs::remove_file(&library).unwrap();
    assert_eq!(stage(&bundle_named).stderr, "error: pinned library missing from the build tree: libA.so\n");
    fs::write(&library, "library A\n").unwrap();
    let small = scratch.root.join("small.pem");
    fs::write(&small, "-----BEGIN CERTIFICATE-----\n".repeat(9)).unwrap();
    let done = stage(&small.display().to_string());
    assert_eq!(done.stderr, format!("error: {} does not look like a CA bundle\n", small.display()));
    assert_eq!(
        scratch.run(&["stage"]).stderr,
        "error: no host CA bundle found; set LOSSLESS_CA_BUNDLE to a PEM file\n"
    );
    fs::write(scratch.root.join("tools/lossless/fairplay-libs.sha256"), "# nothing pinned\n").unwrap();
    let pins = scratch.root.join("tools/lossless/fairplay-libs.sha256").display().to_string();
    assert_eq!(stage(&bundle_named).stderr, format!("error: no pinned hashes in {pins}\n"));
}

#[test]
fn the_service_is_started_seen_and_stopped() {
    let scratch = ScratchVar::new();
    scratch.install_service_launcher();
    fs::create_dir_all(scratch.var.join("wrapper/logs")).unwrap(); // `stage` makes it
    scratch.earlier_sign_in();
    fs::set_permissions(scratch.data.join("DEV_TOKEN"), fs::Permissions::from_mode(0o664)).unwrap();
    scratch.release_port();
    let started_by = |context: &mut Context| {
        context.env.insert("LOSSLESS_DNS".to_string(), "192.0.2.53, 2001:DB8::1 bogus 300.1.1.1".to_string());
    };
    let done = scratch.run_with(&["start"], null(), started_by);
    assert_eq!(done.code, 0, "{}{}", done.stderr, scratch.launcher_log());
    assert_eq!(
        done.stderr,
        "ignoring invalid resolver address 'bogus'\nignoring invalid resolver address '300.1.1.1'\n"
    );
    let report: Value = serde_json::from_str(&done.stdout).unwrap();
    let pid = report["pid"].as_i64().unwrap();
    let status_url = format!("http://127.0.0.1:{}/status", scratch.port);
    let service_log = scratch.data.join("service.log").display().to_string();
    let signed_in = json!({
        "music_token_file": true,
        "storefront_file": true,
        "dev_token_file": true,
        "account_database": false,
        "login_in_progress": false,
    });
    assert_eq!(
        report,
        json!({
            "running": true,
            "pid": pid,
            "port": scratch.port,
            "status_url": status_url,
            "storefronts": ["test"],
            "signed_in": signed_in,
            "resolvers": "192.0.2.53,2001:db8::1",
            "service_log": service_log,
        })
    );
    // the pidfile: one line of JSON, the owner's alone
    let pidfile = scratch.var.join("wrapper/run/service.pid.json");
    assert_eq!(mode(&pidfile), 0o600);
    let line = fs::read_to_string(&pidfile).unwrap();
    let record: Value = serde_json::from_str(&line).unwrap();
    let launcher = scratch.var.join("build/lossless/bin/wrapper-lite-rootless").display().to_string();
    let start_time = Host.proc_start_time(pid as i32).unwrap();
    let started = record["started"].as_str().unwrap();
    assert_eq!(
        line,
        format!(
            "{{\"pid\": {pid}, \"start_time\": {start_time}, \"port\": {}, \"launcher\": \"{launcher}\", \"started\": \
             \"{started}\"}}\n",
            scratch.port
        )
    );
    assert_eq!(started.len(), "2026-01-01T00:00:00+0000".len());
    // how the launcher was started: under its short name, detached, with its own environment
    assert_eq!(Host.proc_cmdline(pid as i32)[0], "wrapper-lite-rootless");
    let service: Value =
        serde_json::from_str(&fs::read_to_string(scratch.runtime("service-report.json")).unwrap()).unwrap();
    let port = scratch.port.to_string();
    assert_eq!(
        service["argv"],
        json!(["--base-dir", "/data", "--host", "127.0.0.1", "--port", port, "--log-file", "/data/service.log"])
    );
    assert_eq!(
        service["env"],
        json!({"HOME": "/data", "LANG": "C.UTF-8", "LITE_DNS_SERVERS": "192.0.2.53,2001:db8::1", "PATH": "/system/bin"})
    );
    assert_eq!(
        (&service["umask"], &service["own_session"], &service["cwd"]),
        (&json!("77"), &json!(true), &json!("wrapper"))
    );
    let launcher_log = scratch.var.join("wrapper/logs/launcher.log");
    assert_eq!(mode(&launcher_log), 0o600);
    let log = fs::read_to_string(&launcher_log).unwrap();
    assert!(
        log.starts_with("\n--- start 20") && log.contains(&format!(" port {}\nservice: listening\n", scratch.port)),
        "{log}"
    );
    assert_eq!(mode(&scratch.data.join("DEV_TOKEN")), 0o600); // tightened

    let done = scratch.run(&["status"]);
    assert_eq!((done.code, done.stderr.as_str()), (0, ""));
    let runtime = scratch.var.join("wrapper").display().to_string();
    assert_eq!(
        serde_json::from_str::<Value>(&done.stdout).unwrap(),
        json!({
            "running": true,
            "started_by_helper": true,
            "pid": pid,
            "port": scratch.port,
            "status_url": status_url,
            "storefronts": ["test"],
            "signed_in": true,
            "account_files": signed_in,
            "runtime": runtime,
            "runtime_staged": true,
            "built": true,
            "service_log": service_log,
        })
    );
    // a second start finds it
    let done = scratch.run(&["start"]);
    assert_eq!((done.code, done.stderr.as_str()), (0, ""));
    assert_eq!(
        done.stdout,
        format!(
            "{{\n  \"running\": true,\n  \"already\": true,\n  \"pid\": {pid},\n  \"port\": {}\n}}\n",
            scratch.port
        )
    );
    assert!(scratch.state().ends_with(" running"));
    // nothing else may use the account while it runs
    let done = scratch.run_with(&["login"], closed_pipe(), |_| {});
    assert_eq!(done.code, 1);
    let mut helper = PipeHelper::start(&scratch, &[]);
    helper.send(json!({"username": USERNAME, "password": "pw-plain"})).unwrap();
    let result = helper.until(None);
    helper.close();
    assert_eq!(result["kind"], "busy");
    assert!(result["error"].as_str().unwrap().starts_with("the wrapper service is running; stop it first"));

    let done = scratch.run(&["stop"]);
    assert_eq!((done.code, done.stderr.as_str()), (0, ""));
    assert_eq!(
        serde_json::from_str::<Value>(&done.stdout).unwrap(),
        json!({
            "running": false,
            "stopped": true,
            "pid": pid,
            "children": [],
            "force_killed": [],
            "remaining": [],
            "port_free": true,
        })
    );
    assert!(!pidfile.exists());
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    let done = scratch.run(&["status"]);
    assert_eq!(done.code, 1);
    let status: Value = serde_json::from_str(&done.stdout).unwrap();
    assert_eq!(
        (&status["running"], &status["started_by_helper"], &status["pid"]),
        (&json!(false), &json!(false), &Value::Null)
    );
    assert_eq!((&status["storefronts"], &status["signed_in"]), (&Value::Null, &json!(false)));
    let done = scratch.run(&["stop"]);
    assert_eq!(done.code, 0);
    assert_eq!(done.stdout, "{\n  \"running\": false,\n  \"stopped\": false,\n  \"note\": \"not running\"\n}\n");
    assert_eq!(mode(&scratch.var.join("wrapper/run/helper.lock")), 0o600);
}

#[test]
fn stop_ends_what_the_service_started_by_force_when_it_must() {
    let scratch = ScratchVar::new();
    scratch.install_service_launcher();
    fs::create_dir_all(scratch.var.join("wrapper/logs")).unwrap();
    // a child of the service that does not end when asked (the service's own PATH has no `sleep`)
    fs::write(scratch.runtime("service-child"), "trap '' TERM; PATH=/usr/bin:/bin; sleep 300\n").unwrap();
    scratch.release_port();
    let done = scratch.run(&["start"]);
    assert_eq!(done.code, 0, "{}{}", done.stderr, scratch.launcher_log());
    let pid = serde_json::from_str::<Value>(&done.stdout).unwrap()["pid"].as_i64().unwrap() as i32;
    // (it runs `sleep` once the shell has read the line that makes it deaf to the request)
    wait_for("the child of the service", || {
        Host.descendants(pid).iter().any(|child| Host.proc_cmdline(*child).first().is_some_and(|name| name == "sleep"))
    });
    let children = Host.descendants(pid);
    // 15 seconds on the helper's clock, less than one on the wall
    let done = scratch.run_with(&["stop"], null(), |context| context.clock = Arc::new(FastClock { factor: 40 }));
    assert_eq!((done.code, done.stderr.as_str()), (0, ""), "{}", done.stdout);
    let report: Value = serde_json::from_str(&done.stdout).unwrap();
    assert_eq!((&report["stopped"], &report["pid"]), (&json!(true), &json!(pid)));
    assert_eq!(report["children"], json!(children));
    assert_eq!(report["force_killed"], json!(children)); // the service itself ended when asked
    assert_eq!((&report["remaining"], &report["port_free"]), (&json!([]), &json!(true)));
    wait_for("the child of the service to be gone", || {
        children.iter().all(|child| !Path::new(&format!("/proc/{child}")).exists())
    });
}

#[test]
fn a_service_that_does_not_come_up_is_explained() {
    let scratch = ScratchVar::new();
    fs::create_dir_all(scratch.var.join("wrapper/logs")).unwrap();
    // the stand-in of the sign-in tests is no service: it ends at once
    fs::write(scratch.runtime("start-fail"), "mount proc failed: Operation not permitted\n").unwrap();
    let done = scratch.run(&["start"]);
    let launcher_log = scratch.var.join("wrapper/logs/launcher.log");
    assert_eq!((done.code, done.stdout.as_str()), (1, ""));
    assert_eq!(
        done.stderr,
        format!(
            "error: the service did not answer on http://127.0.0.1:{}/status; see {} and {}\n",
            scratch.port,
            launcher_log.display(),
            scratch.data.join("service.log").display()
        )
    );
    assert!(fs::read_to_string(&launcher_log).unwrap().ends_with("mount proc failed: Operation not permitted\n"));
    assert!(!scratch.var.join("wrapper/run/service.pid.json").exists());
    // where the wrapper cannot run at all, that is the answer
    fs::write(scratch.runtime("selftest-fail"), "unshare: Operation not permitted\n").unwrap();
    let done = scratch.run_with(&["start"], null(), |context| {
        let settings = HashMap::from([
            ("kernel/unprivileged_userns_clone".to_string(), 0),
            ("user/max_user_namespaces".to_string(), 5),
        ]);
        context.system = Arc::new(Scripted { sysctls: Mutex::new(Some(settings)), ..Scripted::default() });
    });
    assert_eq!(
        done.stderr,
        "error: the wrapper cannot start on this computer: this system did not let it set up the private namespace it runs \
         in (unshare: Operation not permitted); setting in the way: kernel.unprivileged_userns_clone=0\n"
    );
}

#[test]
fn what_stands_in_the_way_of_a_start_is_named() {
    let scratch = ScratchVar::new();
    fs::create_dir_all(scratch.var.join("wrapper/run")).unwrap();
    let pidfile = scratch.var.join("wrapper/run/service.pid.json");
    // a pidfile of a process that is not the launcher (this one): removed, nothing is signalled
    let me = std::process::id();
    fs::write(&pidfile, json!({"pid": me, "start_time": Host.proc_start_time(me as i32)}).to_string()).unwrap();
    let done = scratch.run(&["stop"]);
    assert_eq!(
        done.stdout,
        "{\n  \"running\": false,\n  \"stopped\": false,\n  \"note\": \"stale pidfile removed\"\n}\n"
    );
    assert!(!pidfile.exists());
    for record in ["{\"pid\": 1, \"start_time\": 1}", "{\"pid\": -5}", "{\"pid\": \"x\"}", "not json", "[1]"] {
        fs::write(&pidfile, record).unwrap();
        let done = scratch.run(&["stop"]);
        assert_eq!(done.code, 0, "{record}");
        let status = scratch.run(&["status"]);
        assert_eq!(serde_json::from_str::<Value>(&status.stdout).unwrap()["started_by_helper"], false, "{record}");
    }
    // an argument file of a sign-in
    fs::write(scratch.data.join(".login-args"), "stale\n").unwrap();
    let done = scratch.run(&["start"]);
    let named = scratch.data.join(".login-args").display().to_string();
    assert_eq!(
        done.stderr,
        format!(
            "error: a login argument file exists in the runtime (login in progress or interrupted); finish or clean it up \
             first: {named}\n"
        )
    );
    let done = scratch.run_with(&["login"], closed_pipe(), |context| {
        context.stdin_is_terminal = true; // "is this a terminal" is the context's answer
        context.stdout_is_terminal = true;
    });
    assert_eq!(
        done.stderr,
        format!(
            "error: {named} exists: another login is in progress or was interrupted. If no login is running, remove that \
             file and retry.\n"
        )
    );
    assert_eq!(done.stdout, "");
    fs::remove_file(scratch.data.join(".login-args")).unwrap();
    // a wrapper of someone else on the port
    let server = scratch.status_server(json!({"code": 0, "data": {"regions": []}}));
    let done = scratch.run(&["start"]);
    assert_eq!(
        done.stderr,
        format!(
            "error: another wrapper already answers on 127.0.0.1:{} and was not started by this helper; stop it yourself \
             or choose LOSSLESS_PORT\n",
            scratch.port
        )
    );
    let done = scratch.run(&["stop"]);
    assert_eq!((done.code, done.stdout.as_str()), (2, ""));
    assert_eq!(
        done.stderr,
        format!(
            "a wrapper answers on 127.0.0.1:{} but was not started by this helper; leaving it alone\n",
            scratch.port
        )
    );
    let done = scratch.run(&["status"]);
    assert_eq!(done.code, 0);
    let status: Value = serde_json::from_str(&done.stdout).unwrap();
    assert_eq!((&status["storefronts"], &status["signed_in"]), (&json!([]), &json!(false)));
    assert_eq!(status["note"], "answers on the port but was not started by this helper");
    drop(server);
}

#[test]
fn only_the_wrapper_s_answer_counts_as_running() {
    let scratch = ScratchVar::new();
    let payloads = Arc::new(Mutex::new(json!(null)));
    // one listener for the whole test, its answer changed between the checks
    let socket = scratch.port_hold.lock().unwrap().take().unwrap();
    // SAFETY: listen() on the bound socket taken over just above
    assert_eq!(unsafe { libc::listen(socket.as_raw_fd(), 16) }, 0);
    let listener = TcpListener::from(socket);
    let served = payloads.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let payload = served.lock().unwrap().clone();
            if payload == json!("stop") {
                break;
            }
            answer(stream, &payload);
        }
    });
    for (payload, running) in [
        (json!({"code": 0, "data": {"regions": ["test"]}}), true),
        (json!({"code": 0.0, "data": {"regions": []}}), true),
        (json!({"code": false, "data": {"regions": [1, "x", null]}}), true),
        (json!({"code": 1, "data": {"regions": ["test"]}}), false),
        (json!({"code": 0, "data": {"regions": "test"}}), false),
        (json!({"code": 0}), false),
        (json!({"code": null, "data": {"regions": []}}), false),
        (json!([1]), false),
        (json!("text"), false),
    ] {
        *payloads.lock().unwrap() = payload.clone();
        let done = scratch.run(&["status"]);
        assert_eq!(done.code, if running { 0 } else { 1 }, "{payload}");
        let status: Value = serde_json::from_str(&done.stdout).unwrap();
        assert_eq!(status["running"], running, "{payload}");
        if running {
            assert_eq!(status["storefronts"], payload["data"]["regions"], "{payload}");
        }
    }
    *payloads.lock().unwrap() = json!("stop");
    let _ = TcpStream::connect(("127.0.0.1", scratch.port));
}

#[test]
fn resolvers_come_from_the_environment_or_the_host() {
    let scratch = ScratchVar::new();
    let env_of = |adjust: &dyn Fn(&mut Context)| {
        let err = tempfile::tempfile().unwrap();
        let mut context = scratch.context(null(), null(), err.try_clone().unwrap().into());
        adjust(&mut context);
        let env = wrapper::Helper::new(&context).service_env();
        drop(context);
        (env, read_back(err))
    };
    let pair = |name: &str, value: &str| (name.to_string(), value.to_string());
    let base = [pair("PATH", "/system/bin"), pair("HOME", "/data"), pair("LANG", "C.UTF-8")];
    // the host's list
    let (env, said) = env_of(&|_| {});
    assert_eq!(env, [&base[..], &[pair("LITE_DNS_SERVERS", "192.0.2.53")]].concat());
    assert_eq!(said, "");
    let resolv_conf = scratch.root.join("resolv.conf");
    let lines = "# comment\nsearch example.com\nnameserver 192.0.2.1\nnameserver fe80::1%eth0\nnameserver\n\
                 nameserver not-an-address\nnameserver 2001:DB8::2 extra\nnameserver 192.0.2.3\nnameserver 192.0.2.4\n\
                 nameserver 192.0.2.5\n";
    fs::write(&resolv_conf, lines).unwrap();
    let (env, said) = env_of(&|_| {});
    assert_eq!(env[3], pair("LITE_DNS_SERVERS", "192.0.2.1,fe80::1,2001:db8::2,192.0.2.3")); // four at most
    assert_eq!(said, "ignoring invalid resolver address 'not-an-address'\n");
    // LOSSLESS_DNS in its place
    let (env, said) = env_of(&|context| {
        context.env.insert("LOSSLESS_DNS".to_string(), " 192.0.2.9,192.0.2.010  it's ".to_string());
    });
    assert_eq!(env[3], pair("LITE_DNS_SERVERS", "192.0.2.9"));
    assert_eq!(said, "ignoring invalid resolver address '192.0.2.010'\nignoring invalid resolver address \"it's\"\n");
    // none at all: the wrapper is told nothing, and the person is
    fs::write(&resolv_conf, "search example.com\n").unwrap();
    let (env, said) = env_of(&|_| {});
    assert_eq!(env, base);
    assert_eq!(said, "warning: no host resolver found; the wrapper falls back to its upstream public resolvers\n");
    let (env, _) = env_of(&|context| context.paths.resolv_conf = scratch.root.join("missing"));
    assert_eq!(env, base);
}

#[test]
fn a_cancel_sent_with_the_credentials_ends_the_sign_in_before_it_starts() {
    let scratch = ScratchVar::new();
    scratch.earlier_sign_in();
    let first = format!("{}\n{}\n", json!({"username": USERNAME, "password": "pw-plain"}), json!({"cancel": true}));
    let mut helper = PipeHelper::start_with(&scratch, &[], first.as_bytes(), |_| {});
    let result = helper.until(None);
    let (status, stderr) = helper.close();
    assert_eq!((&result["kind"], status, stderr.as_str()), (&json!("cancelled"), 1, ""));
    assert_eq!(helper.phases(), ["preparing"]);
    assert!(!scratch.runtime("stub-report.json").exists()); // the wrapper never ran
    assert!(scratch.old_account());
}

#[test]
fn what_is_not_a_message_is_passed_over() {
    let scratch = ScratchVar::new();
    let first =
        format!("not json\n[1, 2]\n\n{}\n", json!({"username": format!(" {USERNAME}\t"), "password": "pw-plain"}));
    let mut helper = PipeHelper::start_with(&scratch, &[], first.as_bytes(), |_| {});
    let result = helper.until(None);
    helper.close();
    assert_eq!(result, json!({"event": "result", "ok": true}));
    // the account name went to the wrapper without the blanks around it
    assert_eq!(scratch.report()["args_lines"], 5);
}

/// A `--pipe` sign-in on a clock the test moves.
fn sign_in_on_a_moved_clock(scratch: &ScratchVar) -> (PipeHelper, Arc<MovedClock>) {
    let clock = Arc::new(MovedClock::default());
    let helper = PipeHelper::start_with(scratch, &[], b"", {
        let clock = clock.clone();
        move |context: &mut Context| context.clock = clock
    });
    (helper, clock)
}

/// Move the clock on by `seconds` until the helper has something to say. Once is enough when the
/// wait in question had begun; a wait that begins after the clock was moved needs another step.
fn moved_until_it_tells(helper: &mut PipeHelper, clock: &MovedClock, seconds: f64) -> Value {
    for _ in 0..12 {
        clock.advance(seconds as u64 + 1);
        if let Some(event) = helper.read_event(Duration::from_millis(1500)) {
            return event;
        }
    }
    panic!("the helper went quiet; events so far: {:?}", helper.events);
}

#[test]
fn credentials_that_do_not_come_end_the_sign_in() {
    let scratch = ScratchVar::new();
    scratch.earlier_sign_in();
    let (mut helper, clock) = sign_in_on_a_moved_clock(&scratch);
    assert_eq!(helper.until(Some("preparing"))["phase"], "preparing");
    let result = moved_until_it_tells(&mut helper, &clock, wrapper::PIPE_CREDENTIALS_TIMEOUT_S);
    let (status, stderr) = helper.close();
    assert_eq!(
        result,
        json!({"event": "result", "ok": false, "kind": "cancelled", "error": "Sign-in cancelled", "steps": []})
    );
    assert_eq!((status, stderr.as_str()), (1, ""));
    assert!(!scratch.runtime("stub-report.json").exists());
    assert!(scratch.old_account());
}

#[test]
fn a_wrapper_that_says_nothing_for_too_long_ends_the_sign_in() {
    let scratch = ScratchVar::new();
    scratch.earlier_sign_in();
    let (mut helper, clock) = sign_in_on_a_moved_clock(&scratch);
    helper.send(json!({"username": USERNAME, "password": "pw-hang"})).unwrap();
    helper.until(Some("signing_in"));
    clock.advance(wrapper::PIPE_QUIET_TIMEOUT_S as u64 - 40);
    assert_eq!(helper.read_event(Duration::from_millis(1200)), None); // not yet
    let result = moved_until_it_tells(&mut helper, &clock, wrapper::PIPE_QUIET_TIMEOUT_S);
    let (status, _) = helper.close();
    assert_eq!((&result["kind"], status), (&json!("timeout"), 1));
    assert_eq!(
        result["error"],
        format!("The sign-in did not progress; check the network and start again. Details: {}", scratch.log_shown())
    );
    assert_eq!(result["steps"], json!(["signing_in", "password_request"]));
    let log = fs::read_to_string(scratch.log_file()).unwrap();
    assert!(log.contains("launcher: still running, stopped by the helper"));
    assert!(scratch.old_account());
    let wrapper = scratch.report()["pid"].as_i64().unwrap();
    wait_for("the stand-in wrapper to end", || !Path::new(&format!("/proc/{wrapper}")).exists());
}

#[test]
fn a_code_that_is_not_typed_in_time_ends_the_sign_in() {
    let scratch = ScratchVar::new();
    scratch.earlier_sign_in();
    let (mut helper, clock) = sign_in_on_a_moved_clock(&scratch);
    helper.send(json!({"username": USERNAME, "password": "pw-2fa"})).unwrap();
    helper.until(Some("code_required")); // the wrapper's last word: from here on only time passes
    clock.advance(wrapper::PIPE_QUIET_TIMEOUT_S as u64 + 1);
    assert_eq!(helper.read_event(Duration::from_millis(1200)), None); // waiting for a person takes longer
    let result = moved_until_it_tells(&mut helper, &clock, wrapper::PIPE_CODE_TIMEOUT_S);
    helper.close();
    assert_eq!(
        (&result["kind"], &result["error"]),
        (&json!("timeout"), &json!("No verification code was entered in time; start again"))
    );
    assert_eq!(result["steps"], json!(["signing_in", "password_request", "code_request", "code_prompt"]));
    assert_eq!(result["log"], scratch.log_file().display().to_string()); // written, though the words say it all
    assert_eq!(scratch.report()["codes"], json!([]));
    assert!(scratch.old_account());
}

#[test]
fn a_start_check_that_never_answers_ends() {
    let scratch = ScratchVar::new();
    let launcher = scratch.var.join("build/lossless/bin/wrapper-lite-rootless");
    fs::remove_file(&launcher).unwrap();
    install_script(&launcher, &format!("#!{} -I\n{SILENT_STUB}", python3().display()));
    let stub_pid =
        || fs::read_to_string(scratch.runtime("silent-stub.pid")).ok().and_then(|pid| pid.parse::<i32>().ok());
    let gone = |pid: i32| !Path::new(&format!("/proc/{pid}")).exists();

    // the time limit, on a clock that is moved past it once the launcher runs
    let clock = Arc::new(MovedClock::default());
    let (sender, receiver) = mpsc::channel();
    let done = std::thread::scope(|scope| {
        scope.spawn(|| {
            let done = scratch.run_with(&["selftest"], null(), |context| context.clock = clock.clone());
            sender.send(done).unwrap();
        });
        wait_for("the start check to run the launcher", || stub_pid().is_some());
        loop {
            clock.advance(wrapper::SELFTEST_TIMEOUT_S as u64 + 1);
            if let Ok(done) = receiver.recv_timeout(Duration::from_millis(1500)) {
                break done;
            }
        }
    });
    assert_eq!((done.code, done.stdout.as_str()), (1, ""));
    assert_eq!(
        done.stderr,
        "error: Apple's component for the sign-in and for lossless cannot start on this computer: it did not answer within \
         30 seconds\n"
    );
    assert!(gone(stub_pid().unwrap())); // and its process was ended
    fs::remove_file(scratch.runtime("silent-stub.pid")).unwrap();

    // an interruption (SIGINT, were the helper a program of its own): exit status 130, nothing said
    let (sender, receiver) = mpsc::channel();
    let interrupt = Arc::new(Interrupt::new().unwrap());
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let done = scratch.run_with(&["selftest"], null(), |context| context.interrupt = interrupt.clone());
            sender.send(done).unwrap();
        });
        wait_for("the start check to run the launcher", || stub_pid().is_some());
        interrupt.raise(libc::SIGINT);
    });
    let done = receiver.recv().unwrap();
    assert_eq!((done.code, done.stdout.as_str(), done.stderr.as_str()), (130, "", ""));
    assert!(gone(stub_pid().unwrap()));
}

#[test]
fn the_helper_as_a_program_keeps_its_files_private() {
    let scratch = ScratchVar::new();
    fs::remove_dir_all(scratch.var.join("wrapper")).unwrap();
    fs::remove_dir_all(&scratch.var).unwrap();
    // whatever the umask of who starts it
    let mut command = scratch.process(&["stop"]);
    // SAFETY: umask() is async-signal-safe
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o022);
            Ok(())
        });
    }
    let done = command.stdin(Stdio::null()).output().unwrap();
    assert_eq!(done.status.code(), Some(0), "{}", String::from_utf8_lossy(&done.stderr));
    assert_eq!(
        String::from_utf8(done.stdout).unwrap(),
        "{\n  \"running\": false,\n  \"stopped\": false,\n  \"note\": \"not running\"\n}\n"
    );
    for (path, expected) in
        [("", 0o700), ("wrapper", 0o700), ("wrapper/run", 0o700), ("wrapper/run/helper.lock", 0o600)]
    {
        assert_eq!(mode(&scratch.var.join(path)), expected, "{path}");
    }
}

#[test]
fn a_terminal_sign_in_that_is_interrupted_puts_everything_back() {
    let scratch = ScratchVar::new();
    scratch.earlier_sign_in();
    let (master, slave) = open_pty();
    let mut command = scratch.process(&["login", "--stop-service"]);
    command.stdin(slave.try_clone().unwrap()).stdout(slave.try_clone().unwrap()).stderr(slave);
    // its own session: no controlling terminal, so the hidden prompt reads this pseudo-terminal
    // SAFETY: setsid() is async-signal-safe
    unsafe {
        command.pre_exec(|| if libc::setsid() == -1 { Err(io::Error::last_os_error()) } else { Ok(()) });
    }
    let mut child = command.spawn().unwrap();
    drop(command); // with it this process lets go of the terminal's other end
    let mut master = File::from(master);
    let mut output = Vec::new();
    let mut answered = 0;
    let mut pending = vec![("Apple account email:", USERNAME), ("(hidden):", "pw-2fa")];
    let mut interrupted = false;
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if !wait_readable(master.as_fd(), Duration::from_millis(200)) {
            continue;
        }
        let mut chunk = [0u8; 4096];
        match master.read(&mut chunk) {
            Ok(count) if count > 0 => output.extend_from_slice(&chunk[..count]),
            _ => break,
        }
        if let Some((wanted, line)) = pending.first() {
            if contains(&output[answered..], wanted.as_bytes()) {
                master.write_all(format!("{line}\n").as_bytes()).unwrap();
                answered = output.len();
                pending.remove(0);
            }
        } else if !interrupted && output[answered..].ends_with(b"2FA code: ") {
            // the wrapper waits for the code: Ctrl+C
            // SAFETY: SIGINT to the helper this test started, which has not been waited for
            assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
            interrupted = true;
        }
    }
    let status = child.wait().unwrap();
    let output = String::from_utf8_lossy(&output).into_owned();
    assert!(interrupted, "{output}");
    assert_eq!(status.code(), Some(130), "{output}");
    assert!(output.contains("\nlogin interrupted"), "{output}");
    assert!(!output.contains("pw-2fa"));
    assert!(!scratch.data.join(".login-args").exists());
    assert!(scratch.old_account());
    let report = scratch.report();
    assert_eq!(report.get("codes"), Some(&json!([])));
    let wrapper = report["pid"].as_i64().unwrap();
    wait_for("the stand-in wrapper to end", || !Path::new(&format!("/proc/{wrapper}")).exists());
}
