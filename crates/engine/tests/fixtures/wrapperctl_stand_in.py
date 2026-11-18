"""Stand-in for tools/lossless/wrapperctl.py in the tests of crates/engine/src/session.rs.

The tests copy this file to <scratch>/tools/lossless/wrapperctl.py, and the engine starts it there
the way it starts the real helper. It signs nobody in: no wrapper, no network, no account. It
speaks the two forms of the helper's sign-in,

  login --pipe --stop-service   JSON lines on stdin and stdout (what AndroidSignIn drives)
  login --stop-service          prompts in the terminal it runs in

and the password picks what happens (run_pipe_scenario, terminal_login). Everything it writes lies
under <scratch>/var/wrapper: made-up account files after a "successful" sign-in, a log after some
failed ones, and stub-report.json, which says how it was started. About what was typed the report
holds digests and whether any of it showed up in the arguments or the environment, never the text.
"""
import getpass
import hashlib
import json
import os
import re
import select
import signal
import stat
import sys
import time
from pathlib import Path

ROOT = Path(os.path.abspath(__file__)).parents[2]  # the scratch am-ui folder, spelled as the engine spells it
RUNTIME = ROOT / "var/wrapper"
DATA = RUNTIME / "rootfs/data"
SIGNIN_LOG = RUNTIME / "logs/signin-last.log"
REPORT = RUNTIME / "stub-report.json"
LOGIN_FILES = ("DEV_TOKEN", "MUSIC_TOKEN", "STOREFRONT_ID")
LIFETIME_S = 30.0  # no scenario outlives a failed test by more than this
CANCELLED = "Sign-in cancelled"
NO_PROGRESS = "The sign-in did not progress; check the network and start again"


class Ended(Exception):
    """The sign-in ended without success: `kind` and `message` of the result line."""

    def __init__(self, kind, message, *, log=False):
        super().__init__(message)
        self.kind = kind
        self.message = message
        self.log = log  # leave a sign-in log and name it in the result


# ----------------------------------------------------------------------------- the report

def stream(fd):
    """What a standard stream is connected to."""
    try:
        info = os.fstat(fd)
    except OSError:
        return "closed"
    if os.isatty(fd):
        return "tty"
    if stat.S_ISFIFO(info.st_mode):
        return "pipe"
    if stat.S_ISCHR(info.st_mode) and info.st_rdev == os.stat(os.devnull).st_rdev:
        return "null"
    return "other"


def started():
    mask = os.umask(0o077)
    os.umask(mask)
    return {"argv": sys.argv[1:], "isolated": bool(sys.flags.isolated), "umask": mask,
            "own_session": os.getsid(0) == os.getpid(), "streams": [stream(fd) for fd in (0, 1, 2)], "codes": 0}


def save(report):
    """Replace the report in one step: a test may read it while this process still runs."""
    REPORT.parent.mkdir(parents=True, exist_ok=True)
    temporary = REPORT.with_suffix(".tmp")
    temporary.write_text(json.dumps(report))
    os.replace(temporary, REPORT)


def note_typed(report, username, password):
    """Digests of what was typed, and whether any of it can be seen outside the pipe or the terminal."""
    visible = sys.argv + list(os.environ) + list(os.environ.values())
    report["typed_in_arguments_or_environment"] = any(typed in text for typed in (username, password) if typed
                                                      for text in visible)
    report["username_sha256"] = hashlib.sha256(username.encode()).hexdigest()
    report["password_sha256"] = hashlib.sha256(password.encode()).hexdigest()
    save(report)


# ----------------------------------------------------------------------------- what a sign-in leaves

def fresh_account_files():
    DATA.mkdir(parents=True, exist_ok=True, mode=0o700)
    for name in LOGIN_FILES:
        path = DATA / name
        path.write_text("new-" + name + "\n")
        path.chmod(0o600)


def write_signin_log(outcome, steps):
    SIGNIN_LOG.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    SIGNIN_LOG.write_text(f"sign-in log of the stand-in\nresult: {outcome}\nsteps: {', '.join(steps) or '(none)'}\n")
    SIGNIN_LOG.chmod(0o600)
    return str(SIGNIN_LOG)


# ----------------------------------------------------------------------------- login --pipe

def emit(**event):
    """One JSON line for the engine. A closed pipe is the reader's problem."""
    try:
        sys.stdout.write(json.dumps(event) + "\n")
        sys.stdout.flush()
    except (OSError, ValueError):
        pass


class Control:
    """JSON lines from standard input, with a time limit."""

    def __init__(self):
        self.buffer = b""
        self.closed = False

    def next_message(self, timeout):
        """The next message; None when the input closed or `timeout` passed."""
        deadline = time.monotonic() + timeout
        while True:
            while b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                try:
                    value = json.loads(line)
                except ValueError:
                    continue
                if isinstance(value, dict):
                    return value
            remaining = deadline - time.monotonic()
            if self.closed or remaining <= 0:
                return None
            if select.select([0], [], [], remaining)[0]:
                chunk = os.read(0, 4096)
                if chunk:
                    self.buffer += chunk
                else:
                    self.closed = True


def next_or_end(control, deadline, timed_out):
    """The next message that is not a cancel; the sign-in ends when there is none."""
    message = control.next_message(max(0.0, deadline - time.monotonic()))
    if message is None and not control.closed:
        raise Ended("timeout", timed_out)
    if message is None or message.get("cancel"):
        raise Ended("cancelled", CANCELLED)
    return message


def ask_for_code(control, report, steps):
    """The six digits the engine sends after the "code_required" phase."""
    steps.append("code_prompt")
    emit(event="phase", phase="code_required")
    deadline = time.monotonic() + LIFETIME_S
    while True:
        code = next_or_end(control, deadline, "No verification code was entered in time; start again").get("code")
        if not isinstance(code, str):
            continue
        if not re.fullmatch(r"[0-9]{6}", code):
            emit(event="notice", error="The verification code is six digits")
            continue
        report["codes"] += 1
        save(report)
        steps.append("code_sent")
        emit(event="phase", phase="verifying")
        return code


def noise():
    """Lines the engine has to leave out, and a result whose fields are not what they should be."""
    for line in ("not json", "[1, 2, 3]", '"text"', "{broken", "", "x" * (3 << 19)):
        sys.stdout.write(line + "\n")
    emit(event="notice", error="n" * (3 << 19))  # longer than any line the engine looks at
    sys.stdout.write(" " * (1 << 20) + json.dumps({"event": "notice", "error": "the end of a long line"}) + "\n")
    emit(event="notice", error="p" * ((1 << 20) - 33))  # with its newline exactly the longest line looked at
    emit(event="phase", phase="dancing")  # no such phase
    emit(event="phase")
    emit(event="progress", percent=50)  # no such event
    emit(phase="verifying")  # no event at all
    emit(event="notice", error="A notice", detail="not passed on")
    emit(event="notice")
    emit(event="phase", phase="verifying", detail="not passed on")
    emit(event="result", ok=False, kind="failed", error="an earlier result line", steps=["first"], log="/earlier.log")
    emit(event="result", ok=1, kind=["not", "text"], error="e" * 700, steps=[7] + ["s" * 50] * 49, log=7,
         detail="not passed on")


def run_pipe_scenario(password, control, report, steps):
    """What the "wrapper" does with this password; returns when the sign-in succeeded."""
    if password.startswith("pw-stubborn"):  # takes neither the cancel line nor a signal
        for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            signal.signal(signum, signal.SIG_IGN)
    steps.append("signing_in")
    emit(event="phase", phase="signing_in")
    if password.startswith("pw-plain"):
        pass
    elif password.startswith("pw-2fa"):
        if ask_for_code(control, report, steps) != "123456":
            raise Ended("code", "Apple did not accept the verification code. Start again and enter the new code.",
                        log=True)
    elif password.startswith("pw-wrong"):
        steps.append("password_request")
        raise Ended("credentials", "Apple did not accept the Apple ID or password. Check them before trying again: "
                                   "Apple locks an account after a few wrong passwords.", log=True)
    elif password.startswith("pw-hang"):  # nothing happens until the engine cancels
        deadline = time.monotonic() + LIFETIME_S
        while True:
            next_or_end(control, deadline, NO_PROGRESS)
    elif password.startswith("pw-deaf"):  # does not read the cancel line; a signal ends it like a cancel
        time.sleep(LIFETIME_S)
        raise Ended("timeout", NO_PROGRESS)
    elif password.startswith("pw-stubborn"):
        time.sleep(LIFETIME_S)
        os._exit(1)
    elif password.startswith("pw-linger"):  # stops reporting and goes on running
        os.close(1)
        time.sleep(LIFETIME_S)
        raise Ended("timeout", NO_PROGRESS)
    elif password.startswith("pw-crash"):  # ends without a result line
        os._exit(1)
    elif password.startswith("pw-late"):  # says how it ended and closes its output, then takes its time to clean up
        emit(event="result", ok=False, kind="failed", error="Ended early", steps=steps)
        os.close(1)
        time.sleep(0.5)
        (RUNTIME / "cleaned-up").write_text("")
        os._exit(1)
    elif password.startswith("pw-noise"):
        noise()
        os._exit(1)
    else:
        raise Ended("failed", "The stand-in has no such scenario")
    steps.append("signed_in")
    emit(event="phase", phase="finishing")
    fresh_account_files()


def pipe_login(report):
    control = Control()
    steps = []

    def interrupted(signum, frame):  # the helper ends like a cancel when it is told to terminate
        raise Ended("cancelled", CANCELLED)

    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupted)
    try:
        emit(event="phase", phase="preparing")
        first = control.next_message(LIFETIME_S)
        if first is None or first.get("cancel"):
            raise Ended("cancelled", CANCELLED)
        username, password = first.get("username"), first.get("password")
        if not isinstance(username, str) or not isinstance(password, str) or not username or not password:
            raise Ended("input", "an email and a password are required")
        note_typed(report, username, password)
        run_pipe_scenario(password, control, report, steps)
        emit(event="result", ok=True)
        return 0
    except Ended as end:
        event = {"event": "result", "ok": False, "kind": end.kind, "error": end.message, "steps": steps}
        if end.log:
            event["log"] = write_signin_log(f"{end.kind}: {end.message}", steps)
        emit(**event)
        return 1


# ----------------------------------------------------------------------------- login (terminal)

def terminal_login(report):
    if not sys.stdin.isatty() or not sys.stdout.isatty():
        print("error: run the login from your own terminal; never send passwords or codes through chat",
              file=sys.stderr)
        return 1
    print("Apple account sign-in of the stand-in: nothing typed here goes anywhere.")
    username = input("Apple account email: ").strip()
    password = getpass.getpass("Apple account password (hidden): ")
    note_typed(report, username, password)
    print("\nHanding over to the wrapper. Its own messages follow; answer \"2FA code:\" when it appears.\n",
          flush=True)
    if password == "pw-2fa":
        sys.stdout.write("2FA code: ")
        sys.stdout.flush()
        accepted = sys.stdin.readline().strip() == "123456"
    else:
        accepted = password == "pw-plain"
    if not accepted:
        print("error: login did not produce fresh account tokens; read the wrapper's messages above. "
              "An earlier sign-in on this machine, if any, was kept.", file=sys.stderr)
        return 1
    fresh_account_files()
    print("\nLogin completed: fresh account token files exist in the private runtime (contents not shown).")
    return 0


def main():
    report = started()
    save(report)  # before anything else: a test that expects no sign-in to start looks for this file
    arguments = sys.argv[1:]
    if arguments[:1] != ["login"]:
        print("error: the stand-in only signs in", file=sys.stderr)
        return 2
    return pipe_login(report) if "--pipe" in arguments else terminal_login(report)


if __name__ == "__main__":
    sys.exit(main())
