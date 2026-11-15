#!/usr/bin/env python3
"""am-ui lossless (ALAC) route: control the local FairPlay wrapper service.

The service is WorldObservationLog/wrapper "lite" (commit 386ec82) with the 2026-11-05 TLS repair
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
unread) and moves them back unless the attempt produced fresh ones.
"""
from __future__ import annotations

import argparse
import errno
import fcntl
import getpass
import hashlib
import ipaddress
import json
import os
import pty
import re
import select
import shutil
import signal
import socket
import subprocess
import sys
import termios
import time
import urllib.error
import urllib.request
from collections import deque
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]  # am-ui/
VAR = Path(os.environ.get("AMUI_VAR", str(ROOT / "var"))).expanduser()
BUILD = VAR / "build/lossless"
RUNTIME = VAR / "wrapper"
ROOTFS = RUNTIME / "rootfs"
DATA = ROOTFS / "data"
RUN = RUNTIME / "run"
LOGS = RUNTIME / "logs"
PIDFILE = RUN / "service.pid.json"
LOCKFILE = RUN / "helper.lock"
LAUNCHER = BUILD / "bin/wrapper-lite-rootless"
# What the launcher is started as (argv[0]). It hands its own arguments on to the wrapper, and the
# wrapper ends without a word when argv[0] is 128 characters or longer -- so never the full path, which
# depends on where the am-ui folder lies.
LAUNCHER_ARGV0 = LAUNCHER.name
BUILT_LITE = BUILD / "wrapper-src/rootfs/system/bin/lite"
ONE_ANSWER_MARKER = b"requested again, aborting login"  # a message of patches/0004, present in a wrapper built with it
BUILT_SYSTEM = BUILD / "wrapper-src/rootfs/system"
PIN_FILE = HERE / "fairplay-libs.sha256"
HOST = "127.0.0.1"
PORT = int(os.environ.get("LOSSLESS_PORT", "12351"))
STATUS_URL = f"http://{HOST}:{PORT}/status"
CA_CANDIDATES = (
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/ssl/certs/ca-bundle.crt",
)
# The wrapper appends the six-digit code to the password inside a 64-byte buffer (lite/auth.cpp).
MAX_PASSWORD_BYTES = 63 - 6
LOGIN_FILES = ("DEV_TOKEN", "MUSIC_TOKEN", "STOREFRONT_ID")  # fresh after a successful sign-in
KEPT_FILES = ("MUSIC_TOKEN", "STOREFRONT_ID")  # what the wrapper deletes when a sign-in attempt starts
KEEP_SUFFIX = ".before-login"
# --pipe sign-in: how long each stage may take
PIPE_CREDENTIALS_TIMEOUT_S = 60.0  # the window sends the first line right after starting the helper
PIPE_QUIET_TIMEOUT_S = 240.0  # the wrapper prints nothing while it is not waiting for the code
PIPE_CODE_TIMEOUT_S = 900.0  # waiting for the person to type the code
SELFTEST_TIMEOUT_S = 30.0  # where the wrapper can start at all, its usage text is there within a second
SIGNIN_LOG = LOGS / "signin-last.log"  # the wrapper's last lines of a failed --pipe sign-in, scrubbed
SIGNIN_LOG_LINES = 200


class HelperError(Exception):
    pass


# ----------------------------------------------------------------------------- utilities

def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def proc_start_time(pid: int) -> int | None:
    """Field 22 of /proc/<pid>/stat (start time in clock ticks) or None if the process is gone."""
    try:
        stat = (Path("/proc") / str(pid) / "stat").read_text()
    except OSError:
        return None
    # The command name may contain spaces or parentheses; split after the last ')'.
    fields = stat[stat.rindex(")") + 2:].split()
    return int(fields[19])


def proc_cmdline(pid: int) -> list[str]:
    try:
        raw = (Path("/proc") / str(pid) / "cmdline").read_bytes()
    except OSError:
        return []
    return [part.decode(errors="replace") for part in raw.split(b"\0") if part]


def descendants(pid: int) -> list[int]:
    """Host-namespace PIDs of every descendant (the wrapper's child runs as PID 1 of its own namespace
    but is visible here)."""
    found: list[int] = []
    pending = [pid]
    while pending:
        current = pending.pop()
        task_dir = Path("/proc") / str(current) / "task"
        try:
            threads = list(task_dir.iterdir())
        except OSError:
            continue
        for thread in threads:
            try:
                children = (thread / "children").read_text().split()
            except OSError:
                continue
            for child in children:
                child_pid = int(child)
                if child_pid not in found:
                    found.append(child_pid)
                    pending.append(child_pid)
    return found


def port_free() -> bool:
    """True when nothing listens on HOST:PORT (a connect attempt is refused).

    A bind test is wrong here: connections the stopped wrapper closed stay in TIME-WAIT on this port for
    60 s and block a plain or SO_REUSEADDR bind, while the wrapper itself (cpp-httplib binds with
    SO_REUSEPORT) rebinds over them without trouble. Only a live listener accepts a connection."""
    with socket.socket() as probe:
        probe.settimeout(1.0)
        try:
            probe.connect((HOST, PORT))
        except ConnectionRefusedError:
            return True
        except OSError:
            return False  # unexpected (timeout, filtered); treat as busy
    return False


def tighten_permissions() -> None:
    """Owner-only modes below the data directory. Apple's libraries create some files 0664/0644
    (e.g. adi.pb); the directory is 0700 anyway, this just removes the inconsistency."""
    if not DATA.is_dir():
        return
    for root, dirs, files in os.walk(DATA):
        for name in dirs:
            try:
                os.chmod(os.path.join(root, name), 0o700)
            except OSError:
                pass
        for name in files:
            try:
                os.chmod(os.path.join(root, name), 0o600)
            except OSError:
                pass


def wait_port_free(timeout: float) -> bool:
    """Wait until no listener answers on HOST:PORT (the signalled wrapper may still be shutting down)."""
    deadline = time.monotonic() + timeout
    while True:
        if port_free():
            return True
        if time.monotonic() >= deadline:
            return False
        time.sleep(0.25)


def http_status(timeout: float = 3.0) -> dict | None:
    """The wrapper's /status payload, or None when nothing answers like the wrapper."""
    try:
        with urllib.request.urlopen(STATUS_URL, timeout=timeout) as response:
            payload = json.load(response)
    except (OSError, ValueError, urllib.error.URLError):
        return None
    if not isinstance(payload, dict) or payload.get("code") != 0:
        return None
    regions = (payload.get("data") or {}).get("regions")
    if not isinstance(regions, list):
        return None
    return {"regions": regions}


def read_pidfile() -> dict | None:
    try:
        info = json.loads(PIDFILE.read_text())
    except (OSError, ValueError):
        return None
    if not isinstance(info, dict) or "pid" not in info:
        return None
    return info


def launcher_command(*args: str) -> list[str]:
    """The launcher's argument list; start it with `executable=str(LAUNCHER)`."""
    return [LAUNCHER_ARGV0, "--base-dir", "/data", *args]


def is_wrapper_process(pid: int) -> bool:
    """True when `pid` is a launcher of this build or the wrapper it runs in this runtime, however it
    was started."""
    cmd = proc_cmdline(pid)
    if cmd and cmd[0] == str(LAUNCHER):  # started by the full path, as earlier versions of this helper did
        return True
    try:
        program = os.readlink(f"/proc/{pid}/exe")
    except OSError:
        return False
    if program.endswith(" (deleted)"):  # the file was replaced by a newer build while it runs
        program = program[:-len(" (deleted)")]
    wrapper = ROOTFS / "system/bin/lite"
    return program in (str(LAUNCHER), os.path.realpath(LAUNCHER), str(wrapper), os.path.realpath(wrapper))


def pidfile_process_alive(info: dict) -> bool:
    """True when the recorded launcher is still the same process (pid and start time match)."""
    pid = int(info["pid"])
    start = proc_start_time(pid)
    if start is None or start != info.get("start_time"):
        return False
    cmd = proc_cmdline(pid)
    return bool(cmd) and Path(cmd[0]).name == LAUNCHER.name


def host_resolvers() -> list[str]:
    """Resolvers for the wrapper: LOSSLESS_DNS, else the nameserver lines of /etc/resolv.conf."""
    raw = os.environ.get("LOSSLESS_DNS", "").strip()
    candidates: list[str] = []
    if raw:
        candidates = raw.replace(",", " ").split()
    else:
        try:
            for line in Path("/etc/resolv.conf").read_text().splitlines():
                parts = line.split()
                if len(parts) >= 2 and parts[0] == "nameserver":
                    candidates.append(parts[1].split("%")[0])
        except OSError:
            pass
    valid: list[str] = []
    for item in candidates:
        try:
            valid.append(str(ipaddress.ip_address(item)))
        except ValueError:
            print(f"ignoring invalid resolver address {item!r}", file=sys.stderr)
    return valid[:4]


def service_env() -> dict[str, str]:
    env = {"PATH": "/system/bin", "HOME": "/data", "LANG": "C.UTF-8"}
    resolvers = host_resolvers()
    if resolvers:
        env["LITE_DNS_SERVERS"] = ",".join(resolvers)
    else:
        print("warning: no host resolver found; the wrapper falls back to its upstream public resolvers",
              file=sys.stderr)
    return env


def require_build() -> None:
    if not LAUNCHER.is_file() or not BUILT_LITE.is_file():
        raise HelperError(f"wrapper not built; run {HERE / 'build.sh'} first")


def require_runtime() -> None:
    if not (ROOTFS / "system/bin/lite").is_file() or not (ROOTFS / "system/etc/workspace-ca-bundle.pem").is_file():
        raise HelperError(f"runtime not staged under {RUNTIME}; run {HERE / 'build.sh'} first")


def require_one_answer_wrapper() -> None:
    """A sign-in runs only through a wrapper built with patches/0004 (one answer per credential request).

    An older build repeats a refused password to Apple by itself, and every refusal counts toward
    Apple's account lockout; the checks of this helper come too late to stop that.
    """
    try:
        patched = ONE_ANSWER_MARKER in (ROOTFS / "system/bin/lite").read_bytes()
    except OSError:
        patched = False
    if not patched:
        raise HelperError("The lossless route on this computer was built before its sign-in answered Apple only "
                          "once per attempt (a mistyped password could be sent twice). Update it first: quit "
                          "am-ui, run tools/lossless/stop.sh, then ./setup.sh. A sign-in you already have keeps "
                          "working.")


def shown_path(path: Path) -> str:
    """`path` as one types it in the am-ui folder when it lies inside it."""
    try:
        return str(path.relative_to(ROOT))
    except ValueError:
        return str(path)


def printable(text: str, limit: int = 200) -> str:
    return " ".join("".join(c if c.isprintable() else " " for c in text).split())[:limit]


# The launcher's own steps before the wrapper runs (the names its perror() lines start with).
_NAMESPACE_STEPS = ("unshare", "uid_map", "setgroups", "gid_map", "mount ", "chroot ")
_FOLDER_STEPS = ("mkdir ", "open ", "chdir ")
# A library the Android loader misses whose name carries a version ("libcurl.so.4"): Android libraries
# have none, so the wrapper was linked against a library of the system it was built on.
_FOREIGN_LIBRARY = re.compile(r'library "([^"/]+\.so\.[0-9][^"/]*)" not found')


def userns_settings() -> list[str]:
    """The settings of this system that keep ordinary programs from using user namespaces."""
    found = []
    for name, blocks in (("kernel/apparmor_restrict_unprivileged_userns", lambda value: value != 0),
                         ("kernel/unprivileged_userns_clone", lambda value: value == 0),
                         ("user/max_user_namespaces", lambda value: value == 0)):
        try:
            value = int((Path("/proc/sys") / name).read_text().strip())
        except (OSError, ValueError):
            continue
        if blocks(value):
            found.append(f"{name.replace('/', '.')}={value}")
    return found


def explain_start_failure(lines: list[str]) -> str:
    """Why the wrapper did not start, from what the launcher and the loader printed before it
    (system messages only; nothing of an account is in them)."""
    lines = [printable(line) for line in lines if line.strip()]
    for line in lines:
        low = line.lower()
        if low.startswith(_NAMESPACE_STEPS) and ":" in line:
            settings = userns_settings()
            return (f"this system did not let it set up the private namespace it runs in ({line})"
                    + (f"; setting in the way: {', '.join(settings)}" if settings else ""))
        if low.startswith(_FOLDER_STEPS) and ":" in line:
            return f"it could not prepare its folder under {shown_path(RUNTIME)} ({line})"
        if low.startswith("execve"):
            return (f"this system refused to run its program ({line})"
                    + ("; the am-ui folder may be on a file system mounted noexec" if "permission" in low else ""))
        if "cannot link executable" in low or low.startswith("linker:"):
            foreign = _FOREIGN_LIBRARY.search(line)
            if foreign:  # a build made before build.sh named the rootfs's libraries to CMake
                return (f"it was built against this system's {foreign.group(1)} instead of the library that "
                        f"comes with it, so it cannot be loaded; `./setup.sh` builds it again ({line})")
            return f"its program could not be loaded ({line})"
    if lines:
        return f"it stopped with: {lines[-1]}"
    return "it ended without printing anything (the kernel log, `journalctl -k -n 20`, may say what ended it)"


def launcher_selftest() -> tuple[bool, str]:
    """Run the wrapper just far enough to print its usage text: the launcher's user namespace, its
    mounts and chroot, the Android loader and every library the wrapper links, and nothing else (no
    account file is read, nothing goes to the network). Returns (starts, reason when it does not)."""
    env = {"PATH": "/system/bin", "HOME": "/data", "LANG": "C.UTF-8"}
    try:
        proc = subprocess.Popen(launcher_command("--help"), executable=str(LAUNCHER), cwd=RUNTIME, env=env,
                                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                start_new_session=True)
    except OSError as exc:
        return False, f"its launcher could not be run ({exc.strerror or type(exc).__name__})"
    try:
        output, _ = proc.communicate(timeout=SELFTEST_TIMEOUT_S)
    except subprocess.TimeoutExpired:
        end_launcher(proc)
        proc.stdout.close()
        return False, f"it did not answer within {SELFTEST_TIMEOUT_S:.0f} seconds"
    lines = output.decode("utf-8", "replace").splitlines()
    if any("usage: lite" in line for line in lines):
        return True, ""
    return False, explain_start_failure(lines)


def require_startable() -> None:
    starts, reason = launcher_selftest()
    if not starts:
        raise HelperError(f"Apple's component for the sign-in and for lossless cannot start on this computer: "
                          f"{reason}")


def cmd_selftest(args: argparse.Namespace) -> int:
    require_build()
    require_runtime()
    require_startable()
    print("ok: the wrapper starts on this system")
    return 0


def helper_lock():
    RUN.mkdir(mode=0o700, parents=True, exist_ok=True)
    handle = LOCKFILE.open("a")
    try:
        fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError as exc:
        raise HelperError("another lossless helper command is running in this workspace") from exc
    return handle


def signed_in_state() -> dict:
    """Presence/size-only view of the account files; contents are never read."""
    def present(name: str) -> bool:
        path = DATA / name
        try:
            return path.is_file() and path.stat().st_size > 0
        except OSError:
            return False
    return {
        "music_token_file": present("MUSIC_TOKEN"),
        "storefront_file": present("STOREFRONT_ID"),
        "dev_token_file": present("DEV_TOKEN"),
        "account_database": (DATA / "mpl_db").is_dir() and any((DATA / "mpl_db").iterdir()),
        "login_in_progress": (DATA / ".login-args").exists(),
    }


# ----------------------------------------------------------------------------- commands

def cmd_paths(args: argparse.Namespace) -> int:
    values = {
        "AMUI_LOSSLESS_ROOT": str(ROOT),
        "AMUI_LOSSLESS_BUILD": str(BUILD),
        "AMUI_LOSSLESS_RUNTIME": str(RUNTIME),
        "AMUI_LOSSLESS_DATA": str(DATA),
        "AMUI_LOSSLESS_LAUNCHER": str(LAUNCHER),
        "AMUI_LOSSLESS_PORT": str(PORT),
        "AMUI_LOSSLESS_STATUS_URL": STATUS_URL,
        "AMUI_LOSSLESS_SERVICE_LOG": str(DATA / "service.log"),
    }
    if args.sh:
        for key, value in values.items():
            print(f"export {key}={json.dumps(value)}")
    else:
        print(json.dumps(values, indent=2))
    return 0


def source_fingerprint() -> str:
    """SHA-256 over the files of tools/lossless that decide what the build produces. build.sh stores
    it in the build manifest; a different value later means the build is older than these files."""
    digest = hashlib.sha256()
    files = [HERE / "build.sh", PIN_FILE, HERE / "go/amui-fairplay-rip/main.go", *sorted((HERE / "patches").glob("*.patch"))]
    for path in files:
        digest.update(path.relative_to(HERE).as_posix().encode() + b"\0")
        digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()


def cmd_fingerprint(args: argparse.Namespace) -> int:
    print(source_fingerprint())
    return 0


def build_state() -> str:
    """absent: nothing usable is built. stale: built from other inputs than today's tools/lossless.
    unstaged: built from today's inputs, but the runtime does not hold that build. current: both."""
    outputs = (LAUNCHER, BUILT_LITE, BUILD / "bin/amdl", BUILD / "bin/amui-fairplay-rip")
    if not all(path.is_file() for path in outputs):
        return "absent"
    try:
        recorded = json.loads((BUILD / "manifest.json").read_text()).get("source_fingerprint")
    except (OSError, ValueError, AttributeError):
        recorded = None
    if recorded != source_fingerprint():
        return "stale"
    staged = ROOTFS / "system/bin"
    try:
        same = all(sha256_file(staged / name) == sha256_file(BUILT_SYSTEM / "bin" / name) for name in ("lite", "linker64"))
    except OSError:
        same = False
    if not same or not (ROOTFS / "system/etc/workspace-ca-bundle.pem").is_file() or not (RUNTIME / "amdl/config.yaml").is_file():
        return "unstaged"
    return "current"


def service_running() -> bool:
    """What makes `stage` refuse: something answers on the port, or the launcher of the pidfile lives."""
    info = read_pidfile()
    return http_status() is not None or bool(info and pidfile_process_alive(info))


def cmd_build_state(args: argparse.Namespace) -> int:
    print(build_state(), "running" if service_running() else "stopped")
    return 0


def pinned_hashes() -> dict[str, str]:
    pins: dict[str, str] = {}
    for line in PIN_FILE.read_text().splitlines():
        parts = line.split()
        if len(parts) == 2 and len(parts[0]) == 64 and not line.startswith("#"):
            pins[parts[1]] = parts[0]
    if not pins:
        raise HelperError(f"no pinned hashes in {PIN_FILE}")
    return pins


def copy_if_changed(src: Path, dst: Path, mode: int) -> bool:
    if dst.is_file() and dst.stat().st_size == src.stat().st_size and sha256_file(dst) == sha256_file(src):
        os.chmod(dst, mode)
        return False
    tmp = dst.with_name(dst.name + ".tmp")
    shutil.copyfile(src, tmp)
    os.chmod(tmp, mode)
    os.replace(tmp, dst)
    return True


def cmd_stage(args: argparse.Namespace) -> int:
    require_build()
    with helper_lock():
        if service_running():
            raise HelperError("the service is running; stop it before restaging the runtime")
        VAR.mkdir(mode=0o700, exist_ok=True)
        os.chmod(VAR, 0o700)
        for directory in (RUNTIME, RUN, LOGS, DATA):
            directory.mkdir(mode=0o700, parents=True, exist_ok=True)
            os.chmod(directory, 0o700)
        for directory in (ROOTFS, ROOTFS / "system", ROOTFS / "system/bin", ROOTFS / "system/lib64",
                          ROOTFS / "system/etc"):
            directory.mkdir(mode=0o755, exist_ok=True)

        pins = pinned_hashes()
        manifest: dict = {"staged_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "libraries": {}, "binaries": {}}
        lib_src = BUILT_SYSTEM / "lib64"
        libs = sorted(p for p in lib_src.iterdir() if p.suffix == ".so")
        for name, want in pins.items():
            path = lib_src / name
            if not path.is_file():
                raise HelperError(f"pinned library missing from the build tree: {name}")
            got = sha256_file(path)
            if got != want:
                raise HelperError(f"{name} does not match its pinned SHA-256 ({got}); refusing to stage")
        copied = 0
        for lib in libs:
            copied += copy_if_changed(lib, ROOTFS / "system/lib64" / lib.name, 0o444)
            manifest["libraries"][lib.name] = {"sha256": sha256_file(lib), "pinned": lib.name in pins}
        for name in ("linker64", "lite"):
            src = BUILT_SYSTEM / "bin" / name
            if not src.is_file():
                raise HelperError(f"missing build output {src}")
            copied += copy_if_changed(src, ROOTFS / "system/bin" / name, 0o755)
            manifest["binaries"][name] = sha256_file(src)
        manifest["binaries"]["wrapper-lite-rootless"] = sha256_file(LAUNCHER)

        ca_src = os.environ.get("LOSSLESS_CA_BUNDLE") or next((c for c in CA_CANDIDATES if Path(c).is_file()), None)
        if not ca_src:
            raise HelperError("no host CA bundle found; set LOSSLESS_CA_BUNDLE to a PEM file")
        ca_text = Path(ca_src).read_bytes()
        if ca_text.count(b"BEGIN CERTIFICATE") < 10:
            raise HelperError(f"{ca_src} does not look like a CA bundle")
        copied += copy_if_changed(Path(ca_src), ROOTFS / "system/etc/workspace-ca-bundle.pem", 0o444)
        manifest["ca_bundle"] = {"source": ca_src, "sha256": hashlib.sha256(ca_text).hexdigest(),
                                 "certificates": ca_text.count(b"BEGIN CERTIFICATE")}
        build_manifest = BUILD / "manifest.json"
        if build_manifest.is_file():
            manifest["build"] = json.loads(build_manifest.read_text())
        (RUNTIME / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(json.dumps({"runtime": str(RUNTIME), "files_copied": copied, "libraries": len(libs),
                          "pinned_verified": len(pins), "ca_bundle": ca_src,
                          "account_data_dir": str(DATA), "account_state": signed_in_state()}, indent=2))
    return 0


def spawn_launcher(extra_args: list[str], env: dict[str, str], *, foreground: bool,
                   log_handle=None) -> subprocess.Popen:
    cmd = launcher_command(*extra_args)
    if foreground:
        return subprocess.Popen(cmd, executable=str(LAUNCHER), cwd=RUNTIME, env=env)
    return subprocess.Popen(cmd, executable=str(LAUNCHER), cwd=RUNTIME, env=env, stdin=subprocess.DEVNULL,
                            stdout=log_handle, stderr=subprocess.STDOUT, start_new_session=True)


def cmd_start(args: argparse.Namespace) -> int:
    require_build()
    require_runtime()
    with helper_lock():
        info = read_pidfile()
        if info and pidfile_process_alive(info) and http_status() is not None:
            print(json.dumps({"running": True, "already": True, "pid": info["pid"], "port": PORT}, indent=2))
            return 0
        if (DATA / ".login-args").exists():
            raise HelperError("a login argument file exists in the runtime (login in progress or interrupted); "
                              f"finish or clean it up first: {DATA / '.login-args'}")
        launchers = other_launchers()
        if not launchers:
            restore_account_files()  # a sign-in that was killed may have left the account files moved aside
        if not port_free():
            if http_status() is not None:
                raise HelperError(f"another wrapper already answers on {HOST}:{PORT} and was not started by this "
                                  "helper; stop it yourself or choose LOSSLESS_PORT")
            if not wait_port_free(15.0):
                raise HelperError(f"{HOST}:{PORT} is in use by another program; choose LOSSLESS_PORT")
        if launchers:  # a sign-in in progress uses the same account database
            raise HelperError("a sign-in of this wrapper is still running; start the service once it has ended")
        if info:
            PIDFILE.unlink(missing_ok=True)
        env = service_env()
        launcher_log = LOGS / "launcher.log"
        with launcher_log.open("ab") as log_handle:
            os.fchmod(log_handle.fileno(), 0o600)
            log_handle.write(f"\n--- start {time.strftime('%Y-%m-%dT%H:%M:%S%z')} port {PORT}\n".encode())
            log_handle.flush()
            proc = spawn_launcher(["--host", HOST, "--port", str(PORT), "--log-file", "/data/service.log"],
                                  env, foreground=False, log_handle=log_handle)
        record = {"pid": proc.pid, "start_time": proc_start_time(proc.pid), "port": PORT,
                  "launcher": str(LAUNCHER), "started": time.strftime("%Y-%m-%dT%H:%M:%S%z")}
        PIDFILE.write_text(json.dumps(record) + "\n")
        os.chmod(PIDFILE, 0o600)
        deadline = time.monotonic() + 60
        status = None
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                break
            status = http_status(timeout=1.0)
            if status is not None:
                break
            time.sleep(0.5)
        if status is None:
            PIDFILE.unlink(missing_ok=True)
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGTERM)
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(proc.pid, signal.SIGKILL)
            starts, reason = launcher_selftest()
            if not starts:
                raise HelperError(f"the wrapper cannot start on this computer: {reason}")
            raise HelperError(f"the service did not answer on {STATUS_URL}; see {launcher_log} and "
                              f"{DATA / 'service.log'}")
        tighten_permissions()
        print(json.dumps({"running": True, "pid": proc.pid, "port": PORT, "status_url": STATUS_URL,
                          "storefronts": status["regions"], "signed_in": signed_in_state(),
                          "resolvers": env.get("LITE_DNS_SERVERS", "(upstream defaults)"),
                          "service_log": str(DATA / "service.log")}, indent=2))
    return 0


def stop_service() -> tuple[dict, int]:
    """Stop the service this helper started (never another process). Call with helper_lock() held.
    Returns (report, exit status): 0 stopped or not running, 1 processes remain, 2 a wrapper this
    helper did not start answers on the port."""
    info = read_pidfile()
    if info is None:
        if http_status() is not None:
            return {"running": True, "stopped": False, "note": "not started by this helper; left alone"}, 2
        return {"running": False, "stopped": False, "note": "not running"}, 0
    pid = int(info["pid"])
    if not pidfile_process_alive(info):
        PIDFILE.unlink(missing_ok=True)
        return {"running": False, "stopped": False, "note": "stale pidfile removed"}, 0
    children = descendants(pid)
    targets = [pid, *children]
    try:
        os.killpg(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline and any(proc_start_time(t) is not None for t in targets):
        time.sleep(0.2)
    killed = []
    for target in targets:
        if proc_start_time(target) is not None:
            try:
                os.kill(target, signal.SIGKILL)
                killed.append(target)
            except ProcessLookupError:
                pass
    time.sleep(0.3)
    remaining = [t for t in targets if proc_start_time(t) is not None]
    PIDFILE.unlink(missing_ok=True)
    tighten_permissions()
    port_is_free = wait_port_free(15.0) if not remaining else port_free()
    return ({"running": False, "stopped": True, "pid": pid, "children": children, "force_killed": killed,
             "remaining": remaining, "port_free": port_is_free}, 0 if not remaining else 1)


def cmd_stop(args: argparse.Namespace) -> int:
    with helper_lock():
        report, code = stop_service()
    if code == 2:
        print(f"a wrapper answers on {HOST}:{PORT} but was not started by this helper; leaving it alone",
              file=sys.stderr)
        return 2
    print(json.dumps(report, indent=2))
    return code


def cmd_status(args: argparse.Namespace) -> int:
    info = read_pidfile()
    alive = bool(info and pidfile_process_alive(info))
    status = http_status()
    report = {
        "running": status is not None,
        "started_by_helper": alive,
        "pid": info["pid"] if alive else None,
        "port": PORT,
        "status_url": STATUS_URL,
        "storefronts": status["regions"] if status else None,
        "signed_in": status is not None and bool(status["regions"]),
        "account_files": signed_in_state() if DATA.is_dir() else None,
        "runtime": str(RUNTIME),
        "runtime_staged": (ROOTFS / "system/bin/lite").is_file(),
        "built": LAUNCHER.is_file() and BUILT_LITE.is_file(),
        "service_log": str(DATA / "service.log"),
    }
    if status is None and alive:
        report["note"] = "launcher process alive but /status does not answer"
    elif status is not None and not alive:
        report["note"] = "answers on the port but was not started by this helper"
    print(json.dumps(report, indent=2))
    return 0 if status is not None else 1


LOGIN_NOTICE = """\
Apple's native sign-in for am-ui (the sign-in that also makes lossless available)
---------------------------------------------------------------------------------
What runs: Apple's own Android account/FairPlay libraries inside the local wrapper service, in a
rootless user namespace on this machine. They talk to Apple's servers directly over TLS with
certificate verification on (the host CA store). Nothing is sent anywhere else.

What you will be asked, in this terminal only:
  1. your Apple account email (shown as you type)
  2. your password (hidden)
  3. then the wrapper itself prints "2FA code:" when Apple asks for the six-digit verification
     code from a trusted device. Type the six digits and press Enter.
     If no code shows up on your devices within a minute, press Ctrl+C here: the attempt ends
     and an earlier sign-in on this machine is kept (research/lossless-route.md has the notes).
     One answer per attempt: a refused password or code ends it, so nothing is retried for you.

Where state goes: {data}
  mpl_db/ (Apple's account database), MUSIC_TOKEN, DEV_TOKEN, STOREFRONT_ID, token_cache.json.
  The directory is private (0700). Treat it like a password; never copy it anywhere.
The password is written to a temporary 0600 file inside that directory for the wrapper to read
and deleted when the login process exits. It never appears in a command line or a log.

Success looks like: "login successful" followed by "login complete, exiting", then this helper
confirms fresh token files exist (it does not print them). am-ui then uses this sign-in for the
whole app and starts the lossless service when it needs it. If the attempt fails, an earlier
sign-in on this machine is kept.
"""


def check_credentials(username: str, password: str) -> None:
    """The limits of the wrapper's argument format and password buffer."""
    if not username or not password:
        raise HelperError("an email and a password are required")
    if ":" in username or any(c in username for c in " \t\r\n\0"):
        raise HelperError("the wrapper's argument format cannot carry this email address")
    if any(c in password for c in "\r\n\0") or password != password.rstrip(" \t"):
        raise HelperError("the wrapper's argument format cannot preserve line breaks or trailing spaces in "
                          "this password")
    if len(password.encode()) > MAX_PASSWORD_BYTES:
        raise HelperError(f"this wrapper keeps password plus 2FA code in a 63-byte buffer; passwords longer "
                          f"than {MAX_PASSWORD_BYTES} bytes cannot be used with it")


def other_launchers() -> list[int]:
    """PIDs of this build's launchers and wrappers that do not belong to the recorded service (an
    earlier sign-in that is still running, or a wrapper whose launcher was killed)."""
    info = read_pidfile()
    service = int(info["pid"]) if info and pidfile_process_alive(info) else None
    ours = {os.getpid()}
    if service is not None:
        ours.update((service, *descendants(service)))
    found = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit() or int(entry.name) in ours or not is_wrapper_process(int(entry.name)):
            continue
        if proc_cmdline(int(entry.name))[-1:] == ["--help"]:  # a start check (selftest): it opens nothing
            continue
        found.append(int(entry.name))
    return found


def login_preflight(stop: bool, *, clear_stale_args: bool) -> Path:
    """Checks before a sign-in, with helper_lock() held: nothing else may use the account database.
    Returns the path of the argument file to write."""
    info = read_pidfile()
    if (info and pidfile_process_alive(info)) or http_status() is not None:
        if stop:
            stop_service()
        info = read_pidfile()
        if (info and pidfile_process_alive(info)) or http_status() is not None:
            raise HelperError("the wrapper service is running; stop it first (tools/lossless/stop.sh): the login "
                              "process uses the same account database")
    if not port_free():
        raise HelperError(f"{HOST}:{PORT} is in use; stop whatever listens there before logging in")
    if other_launchers():
        raise HelperError("another sign-in of this wrapper is still running; wait for it to end")
    args_path = DATA / ".login-args"
    if args_path.exists() or args_path.is_symlink():
        if not clear_stale_args:
            raise HelperError(f"{args_path} exists: another login is in progress or was interrupted. "
                              "If no login is running, remove that file and retry.")
        args_path.unlink()  # left by an interrupted sign-in; no sign-in runs (checked above)
    return args_path


def keep_account_files() -> None:
    """Move the files the wrapper deletes at the start of a sign-in aside (rename; contents unread).
    A set kept by an interrupted earlier attempt wins over nothing, never over existing files."""
    for name in KEPT_FILES:
        current, kept = DATA / name, DATA / (name + KEEP_SUFFIX)
        if current.is_file() and not current.is_symlink():
            os.replace(current, kept)


def restore_account_files() -> None:
    """After a failed attempt: put the earlier sign-in's files back where the attempt left none."""
    for name in KEPT_FILES:
        current, kept = DATA / name, DATA / (name + KEEP_SUFFIX)
        if not kept.is_file() or kept.is_symlink():
            continue
        if current.exists():
            kept.unlink()
        else:
            os.replace(kept, current)


def drop_kept_account_files() -> None:
    for name in KEPT_FILES:
        (DATA / (name + KEEP_SUFFIX)).unlink(missing_ok=True)


def fresh_account_files(started_ns: int) -> bool:
    """True when the sign-in wrote every account file after `started_ns` (sizes and times only)."""
    try:
        return all((DATA / name).is_file() and (DATA / name).stat().st_size > 0
                   and (DATA / name).stat().st_mtime_ns >= started_ns for name in LOGIN_FILES)
    except OSError:
        return False


def write_login_args(args_path: Path, username: str, password: str) -> None:
    fd = os.open(args_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "w") as handle:
        handle.write(f"--login\n{username}:{password}\n--base-dir\n/data\n")


def login_env() -> dict[str, str]:
    env = service_env()
    env["LITE_ARGS_FILE"] = "/data/.login-args"
    return env


def end_launcher(proc: subprocess.Popen | None) -> None:
    """Stop a sign-in that is still running: SIGINT to the launcher (it ends its child), then force."""
    if proc is None or proc.poll() is not None:
        return
    targets = {pid: proc_start_time(pid) for pid in descendants(proc.pid)}
    try:
        proc.send_signal(signal.SIGINT)
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
    except (ProcessLookupError, OSError):
        pass
    for pid, started in targets.items():
        if started is not None and proc_start_time(pid) == started:  # still the same process
            try:
                os.kill(pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass


def cmd_login(args: argparse.Namespace) -> int:
    if args.pipe:
        return login_pipe(args)
    require_build()
    require_runtime()
    if args.explain_only:
        print(LOGIN_NOTICE.format(data=DATA))
        return 0
    require_one_answer_wrapper()
    require_startable()
    if not sys.stdin.isatty() or not sys.stdout.isatty():
        raise HelperError("run the login from your own terminal; never send passwords or codes through chat")
    with helper_lock():
        args_path = login_preflight(args.stop_service, clear_stale_args=False)
        print(LOGIN_NOTICE.format(data=DATA))
        username = input("Apple account email: ").strip()
        password = getpass.getpass("Apple account password (hidden): ")
        check_credentials(username, password)

        started_ns = time.time_ns()
        proc = None
        succeeded = False
        try:
            keep_account_files()
            write_login_args(args_path, username, password)
            del password
            print("\nHanding over to the wrapper. Its own messages follow; answer \"2FA code:\" when it appears.\n",
                  flush=True)
            proc = spawn_launcher([], login_env(), foreground=True)
            code = proc.wait()
            succeeded = code == 0 and fresh_account_files(started_ns)
            if not succeeded:
                # The upstream host launcher returns 0 even when the wrapper failed, so the token check decides.
                raise HelperError("login did not produce fresh account tokens; read the wrapper's messages above. "
                                  "An earlier sign-in on this machine, if any, was kept.")
            print("\nLogin completed: fresh account token files exist in the private runtime "
                  "(contents not shown). am-ui uses this sign-in from now on and starts the lossless "
                  "service when it needs it.")
            return 0
        except KeyboardInterrupt:
            print("\nlogin interrupted", file=sys.stderr)
            return 130
        finally:
            end_launcher(proc)
            args_path.unlink(missing_ok=True)
            if succeeded:
                drop_kept_account_files()
            else:
                restore_account_files()
            tighten_permissions()


# ----------------------------------------------------------------------------- login --pipe

class PipeFailure(Exception):
    """A --pipe sign-in ended without success: `kind` for the window, `message` safe to show."""

    def __init__(self, kind: str, message: str, *, says_it_all: bool = False):
        super().__init__(message)
        self.kind = kind
        self.message = message
        self.says_it_all = says_it_all  # nothing to look up in the sign-in log
        self.log: str | None = None  # the sign-in log written for this failure


class LineInput:
    """JSON lines from a file descriptor, read without blocking past what select() announced."""

    def __init__(self, fd: int):
        self.fd = fd
        self.buffer = b""
        self.closed = False

    def feed(self) -> list[dict]:
        """Read what is available; return the complete messages (invalid lines are dropped)."""
        try:
            chunk = os.read(self.fd, 4096)
        except OSError:
            chunk = b""
        if not chunk:
            self.closed = True
            return []
        self.buffer += chunk
        if len(self.buffer) > 16384 and b"\n" not in self.buffer:
            self.buffer = b""  # not our protocol
        messages = []
        while b"\n" in self.buffer:
            line, self.buffer = self.buffer.split(b"\n", 1)
            try:
                value = json.loads(line)
            except ValueError:
                continue
            if isinstance(value, dict):
                messages.append(value)
        return messages

    def next_message(self, timeout: float) -> dict | None:
        """The next message, None when the input closed or `timeout` passed."""
        deadline = time.monotonic() + timeout
        while not self.closed:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return None
            ready, _, _ = select.select([self.fd], [], [], remaining)
            if ready:
                messages = self.feed()
                if messages:
                    return messages[0]
        return None


def emit(**event) -> None:
    """One JSON line for the window. A closed pipe (the window is gone) is the reader's problem."""
    try:
        sys.stdout.write(json.dumps(event) + "\n")
        sys.stdout.flush()
    except (OSError, ValueError):
        pass


_EMAIL_RE = re.compile(r"\S+@\S+")


def safe_apple_message(text: str, username: str) -> str:
    """Apple's own sentence about a refused sign-in, for display only: one line, no account name."""
    text = "".join(c if c.isprintable() else " " for c in text)
    if username:
        text = re.sub(re.escape(username), "your Apple ID", text, flags=re.IGNORECASE)
    text = _EMAIL_RE.sub("your Apple ID", text)
    return " ".join(text.split())[:240]


_ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
_TOKEN_RE = re.compile(r"[A-Za-z0-9+/=_-]{32,}")
_NUMBER_RE = re.compile(r"\d{8,}")
LINE_WITHHELD = "<line left out: it contained something typed into the sign-in>"


def scrub_line(line: str, username: str, typed: list[str]) -> str:
    """One line of the wrapper's output as the sign-in log keeps it: without the Apple ID, e-mail
    addresses, token-like strings and long numbers; a line holding the password or the code is
    dropped whole (replacing it in place would show where it was)."""
    line = "".join(c if c.isprintable() else " " for c in _ANSI_RE.sub("", line)).rstrip()
    if any(secret and secret in line for secret in typed):
        return LINE_WITHHELD
    if username:
        line = re.sub(re.escape(username), "<apple id>", line, flags=re.IGNORECASE)
    line = _EMAIL_RE.sub("<apple id>", line)
    line = _TOKEN_RE.sub("<left out>", line)
    line = _NUMBER_RE.sub("<number>", line)
    return line[:400]


def write_signin_log(outcome: str, steps: list[str], lines: list[str], exit_status: int | None) -> str | None:
    """Keep the scrubbed lines of a failed sign-in for the person to read (0600); the path, None when
    it could not be written."""
    text = ("am-ui sign-in log: what Apple's sign-in component printed during the last failed sign-in.\n"
            "Left out: the Apple ID, e-mail addresses, the password, the verification code, token-like strings\n"
            "and long numbers. Apple's own messages are kept as they came: read it before you share it.\n"
            f"when: {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}\n"
            f"result: {outcome}\n"
            f"steps: {', '.join(steps) or '(none)'}\n"
            f"launcher: {'still running, stopped by the helper' if exit_status is None else f'exit status {exit_status}'}\n"
            f"--- output ({len(lines)} line{'' if len(lines) == 1 else 's'}) ---\n"
            + "".join(line + "\n" for line in lines))
    try:
        LOGS.mkdir(mode=0o700, parents=True, exist_ok=True)
        SIGNIN_LOG.unlink(missing_ok=True)
        fd = os.open(SIGNIN_LOG, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "w") as handle:
            handle.write(text)
    except OSError:
        return None
    return str(SIGNIN_LOG)


def login_pipe(args: argparse.Namespace) -> int:
    """The sign-in for the am-ui window: same wrapper, same checks, no terminal.

    stdin, one JSON object per line:
      {"username": "...", "password": "..."}   first, within a minute
      {"code": "123456"}                       after the "code_required" phase
      {"cancel": true}                         any time; closing stdin cancels too
    stdout, one JSON object per line:
      {"event": "phase", "phase": "preparing" | "signing_in" | "code_required" | "verifying" | "finishing"}
      {"event": "notice", "error": "..."}      a rejected input line (the sign-in goes on)
      {"event": "result", "ok": true}          fresh account files exist; exit status 0
      {"event": "result", "ok": false, "kind": "...", "error": "...", "steps": [...], "log": "..."}
                                               exit status 1; "log" (a path) when one was written
    kinds: input, unavailable, busy, credentials, code, apple, cancelled, timeout, failed.

    The wrapper runs on a pseudo-terminal so that it asks for the code with its terminal prompt (which
    waits as long as needed) and never sees this helper's pipes. What it prints is read here and matched
    against a few fixed markers; its last lines are kept, scrubbed, for the log of a failed attempt
    (SIGNIN_LOG) and are never passed on as they came: Apple's dialog texts can name the account. Before
    anything else the wrapper is started without an account (launcher_selftest): where the system does
    not let it run, the attempt ends as "unavailable" with the reason before the password is used. Two guards
    against Apple's lockout: a second password request means Apple refused the first one, and a second
    code request means it refused the code; either ends the attempt instead of answering again.
    """
    control = LineInput(sys.stdin.fileno())
    steps: list[str] = []

    def interrupted(signum, frame):  # end like a cancel: the clean-up below must run
        raise PipeFailure("cancelled", "Sign-in cancelled")

    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupted)
    try:
        if os.isatty(control.fd):
            raise PipeFailure("input", "--pipe is how the am-ui window signs in; in a terminal run tools/lossless/login.sh")
        try:
            require_build()
            require_runtime()
            require_one_answer_wrapper()
            require_startable()
        except HelperError as exc:
            raise PipeFailure("unavailable", str(exc)) from None
        emit(event="phase", phase="preparing")
        first = control.next_message(PIPE_CREDENTIALS_TIMEOUT_S)
        if first is None or first.get("cancel"):
            raise PipeFailure("cancelled", "Sign-in cancelled")
        username, password = first.get("username"), first.get("password")
        first = None
        if not isinstance(username, str) or not isinstance(password, str):
            raise PipeFailure("input", "an email and a password are required")
        username = username.strip()
        try:
            check_credentials(username, password)
        except HelperError as exc:
            raise PipeFailure("input", str(exc)) from None
        try:
            lock = helper_lock()
        except HelperError as exc:
            raise PipeFailure("busy", str(exc)) from None
        with lock:
            try:
                args_path = login_preflight(args.stop_service, clear_stale_args=True)
            except HelperError as exc:
                raise PipeFailure("busy", str(exc)) from None
            return run_pipe_login(control, args_path, username, password, steps)
    except PipeFailure as failure:
        event = {"event": "result", "ok": False, "kind": failure.kind, "error": failure.message, "steps": steps}
        if failure.log:
            event["log"] = failure.log
        emit(**event)
        return 1
    except Exception as exc:  # noqa: BLE001  (the window must always get a result line)
        emit(event="result", ok=False, kind="failed", error=f"The sign-in helper failed ({type(exc).__name__})",
             steps=steps)
        return 1


def failure_after_exit(seen: dict[str, str], lines: list[str]) -> PipeFailure:
    """Why a sign-in that ran to its end left no account files, from what the wrapper reported."""
    if "apple_message" in seen:
        return PipeFailure("apple", seen["apple_message"], says_it_all=True)
    code = seen.get("auth_code")
    if "dialog" in seen:
        return PipeFailure("apple", f"Apple answered: {seen['dialog']}" + (f" (error {code})" if code else ""))
    if code:
        detail = seen.get("auth_message")
        return PipeFailure("apple", f"Apple refused the sign-in (error {code}{f': {detail}' if detail else ''})")
    if "signed_in" in seen:
        what = seen.get("token_problem")
        return PipeFailure("failed", "Apple accepted the sign-in but "
                           + (f"{what} could not be fetched afterwards" if what else "its token request failed")
                           + "; try again later")
    if "response_type" in seen:
        return PipeFailure("apple", "Apple ended the sign-in without giving a reason "
                                    f"(response type {seen['response_type']})")
    if "started" not in seen:
        return PipeFailure("unavailable", f"Apple's sign-in component did not start: {explain_start_failure(lines)}")
    if "tls_problem" in seen:
        return PipeFailure("failed", f"Apple's sign-in component refused to connect: {seen['tls_problem']}")
    if "contacting" not in seen:
        return PipeFailure("failed", "Apple's sign-in component stopped while starting up, before it contacted Apple")
    if "login_failed" in seen:
        return PipeFailure("failed", "The sign-in ended without an answer from Apple; that is usually the network "
                                     "(a VPN or a DNS filter can block it)")
    return PipeFailure("failed", "Apple's sign-in component stopped unexpectedly while signing in")


def run_pipe_login(control: LineInput, args_path: Path, username: str, password: str, steps: list[str]) -> int:
    started_ns = time.time_ns()
    proc = None
    master = -1
    succeeded = False
    typed = [password]  # never in the log: the password, and the code once it is sent
    lines: deque[str] = deque(maxlen=SIGNIN_LOG_LINES)  # the wrapper's output, scrubbed
    seen: dict[str, str] = {}  # what the wrapper reported, by marker
    try:
        keep_account_files()
        write_login_args(args_path, username, password)
        del password
        master, slave = pty.openpty()
        try:
            attrs = termios.tcgetattr(slave)
            attrs[3] &= ~(termios.ECHO | termios.ECHONL)  # the code is not echoed back to us
            termios.tcsetattr(slave, termios.TCSANOW, attrs)
            proc = subprocess.Popen(launcher_command(), executable=str(LAUNCHER), cwd=RUNTIME, env=login_env(),
                                    stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
        finally:
            os.close(slave)

        pending = b""  # wrapper output not yet ended by a newline
        waiting_for_code = False
        code_sent = False
        password_requests = 0
        quiet_since = time.monotonic()

        def step(name: str) -> None:
            if not steps or steps[-1] != name:
                steps.append(name)

        def ask_for_code() -> None:
            nonlocal waiting_for_code
            if waiting_for_code:
                return
            if code_sent:
                raise PipeFailure("code", "Apple did not accept the verification code. Start again and "
                                          "enter the new code.", says_it_all=True)
            waiting_for_code = True
            step("code_prompt")
            emit(event="phase", phase="code_required")

        def on_line(line: str) -> None:
            nonlocal password_requests
            lines.append(scrub_line(line, username, typed))
            if "wrapper-lite login mode" in line or "logging in..." in line:
                args_path.unlink(missing_ok=True)  # the wrapper has read it
                seen["started"] = "yes"
                if "logging in..." in line:
                    seen["contacting"] = "yes"
                    step("signing_in")
                    emit(event="phase", phase="signing_in")
            elif "credentialHandler:" in line:
                if "2FA: true" in line:
                    step("code_request")
                    if code_sent:
                        raise PipeFailure("code", "Apple did not accept the verification code. Start again and "
                                                  "enter the new code.", says_it_all=True)
                else:
                    step("password_request")
                    password_requests += 1
                    if password_requests > 1:
                        raise PipeFailure("credentials", "Apple did not accept the Apple ID or password. Check "
                                                         "them before trying again: Apple locks an account "
                                                         "after a few wrong passwords.", says_it_all=True)
            elif "dialogHandler:" in line:
                match = re.search(r"dialogHandler: \{title: (.*?), message: (.*)", line)
                title = match.group(1).strip() if match else ""
                if title and title != "Sign In":  # "Sign In" is the dialog the wrapper answers by itself
                    message = match.group(2).strip().removesuffix("}").strip()
                    seen["dialog"] = safe_apple_message(f"{title}: {message}" if message else title, username)
                    step("dialog")
            elif "Enter your 2FA code into" in line or "2FA code timeout" in line:
                raise PipeFailure("failed", "The wrapper did not take the code from this helper; sign in from a "
                                            "terminal instead: tools/lossless/login.sh")
            elif "2FA code:" in line:
                ask_for_code()  # the prompt, with a log line of another thread printed after it
            elif "server message:" in line:
                seen["apple_message"] = safe_apple_message(line.split("server message:", 1)[1], username)
                step("apple_message")
            elif "auth error: code=" in line:
                match = re.search(r"auth error: code=(-?\d+)(?:, message=(.*))?", line)
                if match:
                    seen["auth_code"] = match.group(1)
                    detail = safe_apple_message(match.group(2) or "", username)
                    if detail and detail != "none":
                        seen["auth_message"] = detail
                step("auth_error")
            elif "auth failed: response type" in line:
                match = re.search(r"auth failed: response type (-?\d+)", line)
                seen["response_type"] = match.group(1) if match else "unknown"
                step("auth_failed")
            elif "tls policy:" in line and ("refusing" in line or "missing" in line):
                seen["tls_problem"] = printable(line.split("tls policy:", 1)[1])
                step("tls_problem")
            elif "login successful" in line:
                seen["signed_in"] = "yes"
                step("signed_in")
                emit(event="phase", phase="finishing")
            elif "login failed" in line:
                seen["login_failed"] = "yes"
                step("login_failed")
            elif "login complete, exiting" in line:
                step("complete")
            elif "devToken error" in line or "devtoken:" in line or "failed to get dev token" in line:
                seen["token_problem"] = "the developer token"
                step("token_problem")
            elif "createMusicToken" in line or "failed to get music token" in line:
                seen["token_problem"] = "the music token"
                step("token_problem")
            elif "failed to get storefront ID" in line:
                seen["token_problem"] = "the storefront"
                step("token_problem")

        while True:
            readable = [master] if control.closed else [master, control.fd]
            ready, _, _ = select.select(readable, [], [], 1.0)
            if master in ready:
                try:
                    chunk = os.read(master, 4096)
                except OSError as exc:
                    if exc.errno != errno.EIO:  # EIO: every process on the terminal has exited
                        raise
                    chunk = b""
                if not chunk:
                    break
                quiet_since = time.monotonic()
                pending += chunk
                while b"\n" in pending:
                    raw, pending = pending.split(b"\n", 1)
                    on_line(raw.decode("utf-8", "replace").rstrip("\r"))
                if pending.rstrip().endswith(b"2FA code:"):  # the prompt ends without a newline
                    pending = b""
                    ask_for_code()
                elif len(pending) > 65536:
                    pending = b""
            if control.fd in ready:
                for message in control.feed():
                    if message.get("cancel"):
                        raise PipeFailure("cancelled", "Sign-in cancelled")
                    code = message.get("code")
                    if not waiting_for_code or not isinstance(code, str):
                        continue
                    if not re.fullmatch(r"[0-9]{6}", code):
                        emit(event="notice", error="The verification code is six digits")
                        continue
                    typed.append(code)
                    os.write(master, code.encode() + b"\n")
                    waiting_for_code, code_sent = False, True
                    quiet_since = time.monotonic()
                    step("code_sent")
                    emit(event="phase", phase="verifying")
            if control.closed:
                raise PipeFailure("cancelled", "Sign-in cancelled")
            quiet = time.monotonic() - quiet_since
            if waiting_for_code and quiet > PIPE_CODE_TIMEOUT_S:
                raise PipeFailure("timeout", "No verification code was entered in time; start again",
                                  says_it_all=True)
            if not waiting_for_code and quiet > PIPE_QUIET_TIMEOUT_S:
                raise PipeFailure("timeout", "The sign-in did not progress; check the network and start again")

        if pending.strip():
            on_line(pending.decode("utf-8", "replace").rstrip("\r"))
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            pass
        succeeded = fresh_account_files(started_ns)
        if succeeded:
            SIGNIN_LOG.unlink(missing_ok=True)  # the log of an earlier failed attempt
            emit(event="result", ok=True)
            return 0
        raise failure_after_exit(seen, list(lines))
    except PipeFailure as failure:
        if failure.kind != "cancelled":
            failure.log = write_signin_log(f"{failure.kind}: {failure.message}", steps, list(lines),
                                           proc.poll() if proc is not None else None)
            if failure.log and not failure.says_it_all:
                failure.message = f"{failure.message.rstrip('.')}. Details: {shown_path(SIGNIN_LOG)}"
        raise
    except Exception as exc:  # noqa: BLE001  (reported like any failed attempt, with the log)
        failure = PipeFailure("failed", f"The sign-in helper failed ({type(exc).__name__})")
        failure.log = write_signin_log(f"failed: helper error {type(exc).__name__}", steps, list(lines),
                                       proc.poll() if proc is not None else None)
        if failure.log:
            failure.message = f"{failure.message}. Details: {shown_path(SIGNIN_LOG)}"
        raise failure from None
    finally:
        end_launcher(proc)
        if master >= 0:
            os.close(master)
        args_path.unlink(missing_ok=True)
        if succeeded:
            drop_kept_account_files()
        else:
            restore_account_files()
        tighten_permissions()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    p_paths = sub.add_parser("paths")
    p_paths.add_argument("--sh", action="store_true", help="print shell export lines")
    sub.add_parser("fingerprint")
    sub.add_parser("build-state")
    sub.add_parser("stage")
    sub.add_parser("start")
    sub.add_parser("stop")
    sub.add_parser("status")
    sub.add_parser("selftest")
    p_login = sub.add_parser("login")
    p_login.add_argument("--explain-only", action="store_true", help="print the notice and exit")
    p_login.add_argument("--pipe", action="store_true",
                         help="sign in over JSON lines on stdin/stdout (how the am-ui window runs it)")
    p_login.add_argument("--stop-service", action="store_true",
                         help="stop the service this helper started before signing in")
    args = parser.parse_args()
    os.umask(0o077)
    handler = {"paths": cmd_paths, "fingerprint": cmd_fingerprint, "build-state": cmd_build_state, "stage": cmd_stage,
               "start": cmd_start, "stop": cmd_stop, "status": cmd_status, "selftest": cmd_selftest,
               "login": cmd_login}[args.command]
    return handler(args)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except HelperError as exc:
        print(f"error: {exc}", file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        sys.exit(130)
