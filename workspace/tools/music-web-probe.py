#!/usr/bin/env python3
"""Probe AMDL's web-token Atmos route; does not download or decrypt a song."""
import argparse
import base64
from datetime import datetime, timezone
import getpass
import json
import os
from pathlib import Path
import re
import sys
import tempfile
import time
from urllib.error import HTTPError, URLError
from urllib.parse import parse_qs, urlencode, urljoin, urlsplit, urlunsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener

ROOT = Path(__file__).resolve().parents[1]
SESSION = ROOT / "builds/music-downloader/web-session"
TOKEN_FILE = SESSION / "media-user-token"
API_HOST = "amp-api.music.apple.com"


class ProbeError(Exception):
    pass


class ProbeHTTPError(ProbeError):
    def __init__(self, host, status):
        self.status = status
        super().__init__(f"{host} returned HTTP {status}; account requests are not redirected")


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def validate_url(url, authenticated=False):
    parsed = urlsplit(url)
    host = parsed.hostname or ""
    try:
        port = parsed.port
    except ValueError:
        raise ProbeError("Invalid URL port") from None
    allowed = host in ("music.apple.com", API_HOST) or any(
        host.endswith("." + domain) for domain in ("itunes.apple.com", "mzstatic.com"))
    if (parsed.scheme != "https" or port not in (None, 443) or parsed.username
            or parsed.password or not allowed):
        raise ProbeError("Refused an unexpected URL; no token was sent")
    if authenticated and host != API_HOST:
        raise ProbeError("Account tokens may only be sent to the Apple Music API host")
    return host


def request_headers(url, tokens=None):
    validate_url(url, authenticated=tokens is not None)
    headers = {"User-Agent": "Mozilla/5.0", "Accept": "*/*"}
    if tokens is not None:
        developer, user = tokens
        headers.update({"Origin": "https://music.apple.com", "Referer": "https://music.apple.com/",
                        "Authorization": "Bearer " + developer,
                        "Cookie": "media-user-token=" + user, "Accept": "application/json"})
    return headers


def fetch(url, tokens=None):
    opener = build_opener(NoRedirect())  # HTTPS retains Python's default certificate checks.
    for _ in range(6):
        host = validate_url(url, authenticated=tokens is not None)
        try:
            with opener.open(Request(url, headers=request_headers(url, tokens)), timeout=20) as response:
                data = response.read(20_000_001)
                if len(data) > 20_000_000:
                    raise ProbeError("Response exceeded the probe's size limit")
                return data.decode("utf-8")
        except HTTPError as error:
            location = error.headers.get("Location")
            code = error.code
            error.close()
            if code in (301, 302, 303, 307, 308) and location and tokens is None:
                url = urljoin(url, location)
                continue
            # Do not replay credentials on redirects or print response bodies/URLs.
            raise ProbeHTTPError(host, code) from None
        except URLError as error:
            raise ProbeError(f"Network request to {host} failed ({type(error.reason).__name__})") from None
    raise ProbeError("Too many redirects")


def developer_token():
    html = fetch("https://music.apple.com/us/browse")
    scripts = re.findall(r'<script[^>]+src=["\x27]([^"\x27]+)', html)
    scripts = [urljoin("https://music.apple.com/", src) for src in scripts if "/assets/index" in src]
    for url in dict.fromkeys(scripts[:4]):
        javascript = fetch(url)
        for token in re.findall(r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+", javascript):
            try:
                payload = token.split(".")[1]
                claims = json.loads(base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))
                if claims.get("iss") == "AMPWebPlay" and claims.get("exp", 0) > time.time():
                    return token
            except (ValueError, TypeError):
                continue
    raise ProbeError("Could not locate a current public web developer token; no account request was made")


def track_id(value):
    if re.fullmatch(r"[0-9]+", value):
        return value
    parsed = urlsplit(value)
    if parsed.scheme != "https" or parsed.hostname not in ("music.apple.com", "beta.music.apple.com"):
        raise ProbeError("Use an Apple Music song link or numeric song ID")
    parts = parsed.path.strip("/").split("/")
    selected = parse_qs(parsed.query).get("i", [None])[0]
    if selected is None and len(parts) >= 3 and parts[1] == "song":
        selected = parts[-1]
    if not selected or not re.fullmatch(r"[0-9]+", selected):
        raise ProbeError("Use a single song link, or an album link with an ?i= song selection")
    return selected


def attributes(line):
    return {key: quoted if quoted is not None else plain for key, quoted, plain in
            ((m.group(1), m.group(2), m.group(3)) for m in re.finditer(
                r'([A-Z0-9-]+)=(?:"([^"]*)"|([^,]*))(?:,|$)', line))}


def atmos_variants(text, base_url):
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    if not lines or lines[0] != "#EXTM3U":
        raise ProbeError("Asset response was not an HLS playlist")
    variants = []
    for index, line in enumerate(lines):
        if not line.startswith("#EXT-X-STREAM-INF:"):
            continue
        attrs = attributes(line.split(":", 1)[1])
        if not attrs.get("AUDIO", "").startswith("audio-atmos-"):
            continue
        if "ec-3" not in attrs.get("CODECS", "").split(","):
            continue
        if index + 1 >= len(lines) or lines[index + 1].startswith("#"):
            raise ProbeError("Atmos variant is missing its playlist URL")
        url = urljoin(base_url, lines[index + 1])
        validate_url(url)
        variants.append({"audio_group": attrs["AUDIO"], "codecs": attrs["CODECS"],
                         "bandwidth": attrs.get("BANDWIDTH"), "url": url})
    return variants


def private_write(path, text):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    path.parent.chmod(0o700)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as output:
            temporary = Path(output.name)
            output.write(text)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def validate_user_token(token):
    if not 20 <= len(token) <= 32768 or any(ord(c) < 33 or ord(c) > 126 or c in ';"\\' for c in token):
        raise ProbeError("Expected one cookie value, without cookie name, quotes, spaces or other cookies")
    if token.startswith("media-user-token="):
        raise ProbeError("Paste the cookie value only, without media-user-token=")
    return token


def get_user_token(saved):
    if saved:
        if not TOKEN_FILE.exists():
            raise ProbeError("No saved session. Run: python3 tools/music-web-login.py")
        if TOKEN_FILE.is_symlink() or TOKEN_FILE.stat().st_mode & 0o077:
            raise ProbeError("Saved token must be a private file with mode 0600")
        token = TOKEN_FILE.read_text().strip()
    else:
        if not sys.stdin.isatty():
            raise ProbeError("Run this in your terminal; do not send tokens through chat")
        print("Paste only the media-user-token cookie VALUE. Input is hidden and saved privately in the workspace.")
        token = getpass.getpass("Apple Music browser token (hidden): ").strip()
    validate_user_token(token)
    if not saved:
        private_write(TOKEN_FILE, token + "\n")
    return token


def probe(song, tokens):
    query = urlencode({"id": song, "kind": "song", "includeLicenseUrls": "true",
                       "hlsEncryption": "CBC", "hlsProfile": "enhancedHls"})
    response = json.loads(fetch("https://" + API_HOST + "/v1/play/assets?" + query, tokens))
    assets = response.get("results", {}).get("assets", [])
    if not assets or not isinstance(assets[0].get("url"), str):
        raise ProbeError("Apple returned no usable asset URL for this account/song")
    parsed = urlsplit(assets[0]["url"])
    # Same manifest selection used by AMDL; retain signed query parameters.
    path = re.sub(r"(P\d+)_([^/]+)(\.m3u8)", r"\1_default\3", parsed.path)
    master_url = urlunsplit(parsed._replace(path=path))
    master = fetch(master_url)  # No account headers sent to the CDN.
    variants = atmos_variants(master, master_url)
    if not variants:
        raise ProbeError("No ec-3 Atmos variant advertised. Stopped; stereo fallback is disabled")
    selected = variants[0]
    leaf = fetch(selected["url"])
    if not leaf.lstrip().startswith("#EXTM3U"):
        raise ProbeError("Selected stream response was not an HLS playlist")
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    report = ROOT / "reports" / ("music-web-probe-" + stamp + "-" + str(os.getpid()))
    summary = {"song_id": song, "atmos_advertised": True,
               "variants": [{k: v for k, v in variant.items() if k != "url"} for variant in variants],
               "widevine_key_advertised": "urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed" in leaf,
               "license_tested": False, "audio_downloaded": False, "decoded_16_channels": False}
    private_write(report / "summary.json", json.dumps(summary, indent=2) + "\n")
    private_write(report / "master.private.m3u8", master)
    private_write(report / "stream.private.m3u8", leaf)
    private_write(report / "urls.private.json", json.dumps({"master": master_url, "stream": selected["url"]}) + "\n")
    return summary, report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("song", nargs="?", help="Apple Music song URL or numeric song ID")
    login = parser.add_mutually_exclusive_group()
    login.add_argument("--saved-token", action="store_true", help="reuse the saved browser session (the default)")
    login.add_argument("--paste-token", action="store_true", help="manual hidden-paste fallback instead of the saved session")
    parser.add_argument("--check-public", action="store_true", help="test public developer-token retrieval only")
    args = parser.parse_args()
    os.umask(0o077)
    song = None if args.check_public else track_id(args.song or input("Apple Music song URL: ").strip())
    print("Checking Apple's public web developer token...", flush=True)
    developer = developer_token()
    if args.check_public:
        print("Public developer token found. No account token used or printed.")
        return
    user = get_user_token(not args.paste_token)
    print("Checking the asset API and Atmos playlists...", flush=True)
    summary, report = probe(song, (developer, user))
    print(json.dumps(summary, indent=2))
    print("Private manifest report: " + str(report))
    print("This confirms manifest availability only; license, download and 16-channel rendering remain untested.")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        raise SystemExit(130)
    except ProbeError as error:
        print("Probe stopped: " + str(error), file=sys.stderr)
        raise SystemExit(1)
    except (OSError, ValueError, KeyError, TypeError, AttributeError) as error:
        # Raw exceptions can contain signed URLs or portions of API responses.
        print("Probe stopped (" + type(error).__name__ + "). No secret values were logged.", file=sys.stderr)
        raise SystemExit(1)
