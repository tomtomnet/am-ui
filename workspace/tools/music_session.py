"""Validated browser-token storage and minimal read-only library access checks."""
from datetime import datetime, timezone
import importlib.util
import json
import math
from pathlib import Path
import time

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("music_session_web", ROOT / "tools/music-web-probe.py")
web = importlib.util.module_from_spec(spec)
spec.loader.exec_module(web)
SESSION = web.SESSION
TOKEN_FILE = web.TOKEN_FILE
STATE_FILE = SESSION / "session-status.json"
BROWSER_PROFILE = SESSION / "chromium-profile"


def now():
    return datetime.now(timezone.utc).isoformat()


def choose_cookie(cookies):
    """Only consider the Music cookie returned for the explicit Apple Music URL."""
    candidates = []
    for cookie in cookies:
        if (cookie.get("name") != "media-user-token"
                or cookie.get("domain", "").lstrip(".") not in {"music.apple.com", "apple.com"}
                or cookie.get("path", "/") != "/"):
            continue
        expiry = cookie.get("expires", -1)
        if (not isinstance(expiry, (int, float)) or not math.isfinite(expiry)
                or expiry > 253402300799 or (expiry >= 0 and expiry <= time.time())):
            continue
        value = cookie.get("value")
        if not isinstance(value, str):
            continue
        try:
            web.validate_user_token(value)
        except web.ProbeError:
            continue
        candidates.append(cookie)
    return max(candidates, key=lambda c: len(c["domain"]), default=None)


def verify_user(token, developer=None):
    """Only one-item metadata pages; no media, full library, names or IDs are saved."""
    web.validate_user_token(token)
    developer = developer or web.developer_token()
    access = {}
    for kind in ("playlists", "songs"):
        body = json.loads(web.fetch(f"https://{web.API_HOST}/v1/me/library/{kind}?limit=1", (developer, token)))
        if not isinstance(body.get("data"), list):
            raise web.ProbeError("Apple did not return the expected library response")
        access[kind] = {"accessible": True, "sample_count": len(body["data"]), "has_more": bool(body.get("next"))}
    return {"state": "connected", "checked_at": now(), "library": access}


def check_saved():
    try:
        result = verify_user(web.get_user_token(True))
    except web.ProbeHTTPError as error:
        if error.status in (401, 403):
            web.private_write(STATE_FILE, json.dumps({"state": "reconnect_required", "checked_at": now(),
                                                     "http_status": error.status}, indent=2) + "\n")
        raise
    if STATE_FILE.is_file() and not STATE_FILE.is_symlink():
        try:
            old = json.loads(STATE_FILE.read_text())
            # Only copy nonsecret metadata created by this helper.
            for key in ("captured_at", "cookie_expires_at", "source"):
                if key in old:
                    result[key] = old[key]
        except (ValueError, OSError):
            pass
    web.private_write(STATE_FILE, json.dumps(result, indent=2) + "\n")
    return result


def accept_cookie(cookie, *, developer=None):
    if choose_cookie([cookie]) is None:
        raise web.ProbeError("No usable Apple Music session cookie was found")
    result = verify_user(cookie["value"], developer)
    expiry = cookie.get("expires", -1)
    result.update({"source": "dedicated-chromium-profile", "captured_at": now(),
                   "cookie_expires_at": datetime.fromtimestamp(expiry, timezone.utc).isoformat() if expiry > 0 else None})
    # The old working token is preserved if candidate validation fails.
    web.private_write(TOKEN_FILE, cookie["value"] + "\n")
    web.private_write(STATE_FILE, json.dumps(result, indent=2) + "\n")
    return result
