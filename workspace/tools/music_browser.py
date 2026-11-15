"""Private Chromium DevTools pipe for the dedicated Apple Music sign-in profile.

No TCP debug port, ordinary-browser profile access, network-event recording,
credential logging, certificate overrides, or browser sandbox overrides.
"""
import fcntl
import json
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]


class BrowserError(Exception):
    pass


def private_directory(path):
    if path.is_symlink():
        raise BrowserError("Refused a symlink for the browser's private directory")
    path.mkdir(parents=True, mode=0o700, exist_ok=True)
    path.chmod(0o700)


class BrowserPipe:
    def __init__(self, profile, *, headless=False):
        binary = next((p for name in ("chromium-browser", "chromium", "google-chrome")
                       if (p := shutil.which(name))), None)
        if not binary:
            raise BrowserError("Chromium is required for the sign-in window")
        if not headless and not (os.environ.get("DISPLAY") or os.environ.get("WAYLAND_DISPLAY")):
            raise BrowserError("Run the sign-in command from a terminal in your desktop session")
        private_directory(profile)
        # Chromium's Unix socket path has a 108-byte limit; keep scratch local and short.
        scratch = ROOT / "builds/mb-tmp"
        private_directory(scratch)
        command = [binary, f"--user-data-dir={profile}", "--remote-debugging-pipe",
                   "--no-first-run", "--no-default-browser-check", "--disable-sync",
                   "--disable-background-networking", "--disable-component-update",
                   "--disable-session-crashed-bubble", "--window-size=1100,850"]
        if headless:
            command.extend(["--headless=new", "--mute-audio"])
        command.append("about:blank")
        read_in, self.write_fd = os.pipe()
        self.read_fd, write_out = os.pipe()
        env = dict(os.environ, TMPDIR=str(scratch))
        self.sequence = 0
        self.buffer = b""
        self.selector = selectors.DefaultSelector()
        try:
            self.process = subprocess.Popen(
                [sys.executable, str(Path(__file__).resolve()), "--pipe-exec", str(read_in), str(write_out), *command],
                pass_fds=(read_in, write_out), env=env, stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except BaseException:
            os.close(self.write_fd); os.close(self.read_fd); self.selector.close()
            raise
        finally:
            os.close(read_in); os.close(write_out)
        self.selector.register(self.read_fd, selectors.EVENT_READ)
        try:
            self.call("Browser.getVersion", timeout=20)
        except BaseException:
            self.close()
            raise

    def call(self, method, params=None, *, session=None, timeout=12):
        self.sequence += 1
        request = {"id": self.sequence, "method": method, "params": params or {}}
        if session:
            request["sessionId"] = session
        encoded = json.dumps(request).encode() + b"\0"
        try:
            while encoded:
                count = os.write(self.write_fd, encoded)
                encoded = encoded[count:]
        except (BrokenPipeError, OSError):
            raise BrowserError("The sign-in browser has closed") from None
        deadline = time.monotonic() + timeout
        while True:
            while b"\0" in self.buffer:
                message, self.buffer = self.buffer.split(b"\0", 1)
                if not message:
                    continue
                value = json.loads(message)
                if value.get("id") != self.sequence:
                    continue  # Do not record browser events or page content.
                if "error" in value:
                    raise BrowserError(f"Browser command {method} failed")
                return value.get("result", {})
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not self.selector.select(remaining):
                raise BrowserError("The sign-in browser did not respond in time")
            chunk = os.read(self.read_fd, 65536)
            if not chunk:
                raise BrowserError("The sign-in browser has closed")
            self.buffer += chunk
            if len(self.buffer) > 4 * 1024 * 1024:
                raise BrowserError("Browser response exceeded the sign-in helper's limit")

    def page(self, url="about:blank"):
        pages = self.call("Target.getTargets")["targetInfos"]
        page = next((p for p in pages if p["type"] == "page" and p.get("url") == "about:blank"), None)
        target = page["targetId"] if page else self.call("Target.createTarget", {"url": "about:blank"})["targetId"]
        session = self.call("Target.attachToTarget", {"targetId": target, "flatten": True})["sessionId"]
        if url != "about:blank":
            self.call("Page.navigate", {"url": url}, session=session)
        return session

    def music_cookies(self, session):
        return self.call("Network.getCookies", {"urls": ["https://music.apple.com/"]}, session=session)["cookies"]

    def close(self):
        if self.process.poll() is None:
            try:
                self.call("Browser.close", timeout=3)
            except (BrowserError, OSError):
                pass
            try:
                self.process.wait(5)
            except subprocess.TimeoutExpired:
                self.process.terminate()
                try:
                    self.process.wait(3)
                except subprocess.TimeoutExpired:
                    self.process.kill(); self.process.wait()
        self.selector.close()
        for fd in (self.write_fd, self.read_fd):
            try:
                os.close(fd)
            except OSError:
                pass

    def __enter__(self):
        return self

    def __exit__(self, *unused):
        self.close()


def pipe_exec():
    # Remap only in this child before exec, without unsafe preexec_fn in a threaded parent.
    source_in = fcntl.fcntl(int(sys.argv[2]), fcntl.F_DUPFD_CLOEXEC, 10)
    source_out = fcntl.fcntl(int(sys.argv[3]), fcntl.F_DUPFD_CLOEXEC, 10)
    os.dup2(source_in, 3, inheritable=True)
    os.dup2(source_out, 4, inheritable=True)
    for fd in {source_in, source_out, int(sys.argv[2]), int(sys.argv[3])} - {3, 4}:
        os.close(fd)
    os.execv(sys.argv[4], sys.argv[4:])


if __name__ == "__main__":
    if len(sys.argv) > 4 and sys.argv[1] == "--pipe-exec":
        pipe_exec()
