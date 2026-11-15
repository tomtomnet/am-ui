#!/usr/bin/env python3
"""Connect Apple Music through its real website; capture the Music cookie automatically.

Default: reuse and check a saved session, otherwise open the sign-in browser.
--login opens the dedicated browser even when the saved token works.
--check verifies the saved token and library access without opening a browser.
The password and 2FA are entered on Apple's website, never in this helper.
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import sys
import time

from music_browser import BrowserError, BrowserPipe, private_directory
import music_session as session

ROOT = Path(__file__).resolve().parents[1]


def capture(timeout):
    private_directory(session.SESSION)
    descriptor = os.open(session.SESSION / ".login.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise BrowserError("An Apple Music sign-in window is already running") from None
        print("Opening Apple Music in a dedicated Chromium window.", flush=True)
        print("Click Sign In there and complete Apple's normal sign-in. The session will be saved automatically.", flush=True)
        developer = session.web.developer_token()
        with BrowserPipe(session.BROWSER_PROFILE) as browser:
            page = browser.page("https://music.apple.com/")
            deadline = time.monotonic() + timeout
            rejected_value = None
            retry_after = 0
            while time.monotonic() < deadline:
                cookie = session.choose_cookie(browser.music_cookies(page))
                if cookie and (cookie["value"] != rejected_value or time.monotonic() >= retry_after):
                    print("Apple Music session detected; checking library access…", flush=True)
                    try:
                        return session.accept_cookie(cookie, developer=developer)
                    except session.web.ProbeHTTPError as error:
                        if error.status not in (401, 403):
                            raise
                        rejected_value = cookie["value"]
                        retry_after = time.monotonic() + 30
                        print("Apple hasn't accepted this session yet. Complete sign-in in the browser; the existing saved token is unchanged.", flush=True)
                time.sleep(1)
        raise BrowserError("Sign-in timed out. Run the command again when ready; the existing saved token is unchanged")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--login", action="store_true", help="open the browser to capture or reconnect the session")
    mode.add_argument("--check", action="store_true", help="verify the saved session without opening a browser")
    parser.add_argument("--timeout", type=int, default=900, help="seconds allowed for interactive sign-in (default 900)")
    args = parser.parse_args()
    if not 15 <= args.timeout <= 3600:
        parser.error("--timeout must be between 15 and 3600 seconds")
    os.umask(0o077)
    report = ROOT / "reports/music-login" / (time.strftime("%Y%m%dT%H%M%SZ", time.gmtime()) + f"-{os.getpid()}")
    report.mkdir(parents=True, mode=0o700)
    result = None
    try:
        if not args.login and (args.check or session.TOKEN_FILE.exists()):
            print("Checking the saved Apple Music session…", flush=True)
            try:
                result = session.check_saved()
            except session.web.ProbeHTTPError as error:
                if args.check or error.status not in (401, 403):
                    raise
                print("Apple did not accept the saved session. Reconnect in the sign-in window.", flush=True)
        if result is None:
            result = capture(args.timeout)
        print("Connected. Your library songs and playlists are accessible.", flush=True)
        print("The player helpers will reuse this saved session; no cookie copying is needed.", flush=True)
        result = {"ok": True, **result}
    except (session.web.ProbeError, BrowserError) as error:
        result = {"ok": False, "error": str(error)}
        print("Connection stopped: " + str(error), file=sys.stderr)
    except KeyboardInterrupt:
        result = {"ok": False, "error": "Sign-in cancelled"}
        print("Sign-in cancelled. No unverified token was saved.", file=sys.stderr)
    except (OSError, ValueError, KeyError, TypeError) as error:
        result = {"ok": False, "error": type(error).__name__}
        print("Connection stopped (" + type(error).__name__ + "). No secret values were logged.", file=sys.stderr)
    finally:
        if result is not None:
            session.web.private_write(report / "summary.json", json.dumps(result, indent=2) + "\n")
    return 0 if result and result.get("ok") else 1


if __name__ == "__main__":
    raise SystemExit(main())
