#!/usr/bin/env python3
"""Credential-free TLS verification probe for the lossless-route wrapper.

Runs the built wrapper from a THROWAWAY runtime copy (no account data, fresh data directory) with
`--proxy http://127.0.0.1:<port>` pointing at a local HTTP CONNECT proxy that terminates TLS itself
with a freshly generated self-signed certificate whose name matches the requested host. The only way
a client can accept that certificate is to skip CA verification, so:

  every CONNECT whose handshake the wrapper aborts  -> verification active for that destination
  any handshake that completes / any plaintext seen -> verification OFF (reported as FAIL)

Nothing reaches Apple during the probe: the proxy never connects upstream. The wrapper's own libcurl
path (music.apple.com developer-token scrape) and the Apple libraries' startup and /m3u8 requests
are exercised, logged out. The real runtime under var/wrapper is not used or modified.

Usage: python3 -I tools/lossless/probe-tls.py [--seconds 25] [--keep]
Writes var/build/lossless/probe-tls-<timestamp>.json (0600) and prints a summary; exit 0 on PASS.
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import wrapperctl  # noqa: E402  (same directory; python -I does not add it)


class ProbeProxy(threading.Thread):
    """HTTP CONNECT proxy that answers every tunnel with a self-signed certificate for the requested host."""

    def __init__(self, certdir: Path):
        super().__init__(daemon=True)
        self.certdir = certdir
        self.events: list[dict] = []
        self.lock = threading.Lock()
        self.sock = socket.socket()
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(16)
        self.port = self.sock.getsockname()[1]
        self.stopping = False

    def cert_for(self, host: str) -> tuple[Path, Path]:
        safe = "".join(c if c.isalnum() or c in ".-" else "_" for c in host)[:200]
        cert, key = self.certdir / f"{safe}.crt", self.certdir / f"{safe}.key"
        if not cert.exists():
            subprocess.run(["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
                            "-nodes", "-days", "2", "-keyout", str(key), "-out", str(cert),
                            "-subj", f"/CN={host}", "-addext", f"subjectAltName=DNS:{host}"],
                           check=True, capture_output=True)
        return cert, key

    def record(self, event: dict) -> None:
        event["t"] = round(time.monotonic() - self.t0, 3)
        with self.lock:
            self.events.append(event)

    def run(self) -> None:
        self.t0 = time.monotonic()
        self.sock.settimeout(0.5)
        while not self.stopping:
            try:
                conn, _ = self.sock.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            threading.Thread(target=self.handle, args=(conn,), daemon=True).start()

    def handle(self, conn: socket.socket) -> None:
        conn.settimeout(10)
        try:
            head = b""
            while b"\r\n\r\n" not in head and len(head) < 8192:
                chunk = conn.recv(4096)
                if not chunk:
                    break
                head += chunk
            request_line = head.split(b"\r\n", 1)[0].decode(errors="replace")
            parts = request_line.split()
            if len(parts) < 2:
                self.record({"kind": "garbage", "bytes": len(head)})
                return
            method, target = parts[0], parts[1]
            if method != "CONNECT":
                # A plain http:// request through the proxy: record the host only, never the headers.
                host = target.split("//", 1)[-1].split("/", 1)[0]
                self.record({"kind": "plain_http", "method": method, "host": host})
                conn.sendall(b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
                return
            host = target.rsplit(":", 1)[0]
            conn.sendall(b"HTTP/1.1 200 Connection established\r\n\r\n")
            cert, key = self.cert_for(host)
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(str(cert), str(key))
            event = {"kind": "connect", "host": host}
            try:
                tls = context.wrap_socket(conn, server_side=True)
            except ssl.SSLError as exc:
                event.update(handshake="rejected_by_client", detail=exc.reason or str(exc))
                self.record(event)
                return
            except (ConnectionResetError, BrokenPipeError, OSError, EOFError) as exc:
                event.update(handshake="closed_by_client", detail=type(exc).__name__)
                self.record(event)
                return
            # The client accepted an untrusted certificate. Count plaintext but never store it.
            tls.settimeout(3)
            seen = 0
            looks_http = False
            try:
                first = tls.recv(4096)
                seen += len(first)
                looks_http = first[:8].split(b" ")[0].isalpha() if first else False
                while True:
                    more = tls.recv(4096)
                    if not more:
                        break
                    seen += len(more)
            except (socket.timeout, ssl.SSLError, OSError):
                pass
            event.update(handshake="COMPLETED", plaintext_bytes=seen, looks_like_http=looks_http,
                         tls_version=tls.version())
            self.record(event)
            try:
                tls.sendall(b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
            except OSError:
                pass
        finally:
            try:
                conn.close()
            except OSError:
                pass


def http_get(url: str, timeout: float) -> tuple[int | None, str]:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as response:
            return response.status, response.read(2000).decode(errors="replace")
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read(2000).decode(errors="replace")
    except Exception as exc:  # noqa: BLE001
        return None, f"{type(exc).__name__}: {exc}"


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--seconds", type=float, default=25.0, help="how long to let the wrapper run after /status")
    parser.add_argument("--keep", action="store_true", help="keep the throwaway runtime for inspection")
    parser.add_argument("--direct", action="store_true",
                        help="no MITM proxy: talk to Apple directly (logged out) to check that verified requests "
                             "still succeed; pair with --debug-log to see the libraries' own HTTP/TLS log lines")
    parser.add_argument("--debug-log", action="store_true",
                        help="run the wrapper at log level debug (throwaway runtime only; it holds no account data)")
    parser.add_argument("--exercise-m3u8", action="store_true",
                        help="also GET /m3u8 for one song. Logged out, Apple's library then runs an authenticate "
                             "action that the wrapper answers with EMPTY credentials (auth.itunes.apple.com answers "
                             "-5000); harmless but not needed for the TLS check, so off by default")
    args = parser.parse_args()
    os.umask(0o077)
    wrapperctl.require_build()
    wrapperctl.require_runtime()

    probe_root = wrapperctl.BUILD / "probe-runtime"
    if probe_root.exists():
        shutil.rmtree(probe_root)
    (probe_root / "rootfs/data").mkdir(parents=True, mode=0o700)
    subprocess.run(["cp", "-a", "--reflink=auto", str(wrapperctl.ROOTFS / "system"), str(probe_root / "rootfs/system")],
                   check=True)
    for name in ("MUSIC_TOKEN", "DEV_TOKEN", "STOREFRONT_ID", ".login-args"):
        assert not (probe_root / "rootfs/data" / name).exists()

    certdir = Path(tempfile.mkdtemp(prefix="probe-certs-", dir=str(wrapperctl.BUILD / "tmp")))
    proxy = ProbeProxy(certdir)
    proxy.start()
    service_port = free_port()
    status_url = f"http://127.0.0.1:{service_port}/status"
    report: dict = {"started": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "proxy_port": proxy.port,
                    "service_port": service_port, "runtime": str(probe_root), "requests": []}
    env = {"PATH": "/system/bin", "HOME": "/data", "LANG": "C.UTF-8"}
    cmd = [str(wrapperctl.LAUNCHER), "--base-dir", "/data", "--host", "127.0.0.1", "--port", str(service_port),
           "--log-file", "/data/service.log"]
    if args.direct:
        # Real endpoints, so the host resolvers are needed; the proxy stays idle and must see nothing.
        env.update({k: v for k, v in wrapperctl.service_env().items() if k == "LITE_DNS_SERVERS"})
    else:
        # No LITE_DNS_SERVERS on purpose: every https:// request must go through the proxy, which never
        # resolves or contacts anything, so a resolver would only matter for traffic that bypasses it.
        cmd += ["--proxy", f"http://127.0.0.1:{proxy.port}"]
    if args.debug_log:
        cmd += ["--log-level", "debug"]
    report["mode"] = "direct" if args.direct else "mitm-proxy"
    launcher_log = (probe_root / "launcher.log").open("ab")
    proc = subprocess.Popen(cmd, cwd=probe_root, env=env, stdin=subprocess.DEVNULL, stdout=launcher_log,
                            stderr=subprocess.STDOUT, start_new_session=True)
    try:
        deadline = time.monotonic() + 60
        up = False
        while time.monotonic() < deadline and proc.poll() is None:
            code, _ = http_get(status_url, 1.0)
            if code == 200:
                up = True
                break
            time.sleep(0.5)
        report["service_up"] = up
        if up:
            paths = ["/status"]
            if args.exercise_m3u8:
                paths.append("/m3u8?adamId=1234567890")
            for path in paths:
                code, body = http_get(f"http://127.0.0.1:{service_port}{path}", 30.0)
                report["requests"].append({"path": path, "http": code, "body_head": body[:160]})
            time.sleep(args.seconds)
    finally:
        children = wrapperctl.descendants(proc.pid)
        try:
            os.killpg(proc.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        end = time.monotonic() + 15
        while time.monotonic() < end and any(wrapperctl.proc_start_time(p) is not None for p in [proc.pid, *children]):
            time.sleep(0.2)
        for pid in [proc.pid, *children]:
            if wrapperctl.proc_start_time(pid) is not None:
                os.kill(pid, signal.SIGKILL)
        proc.wait(timeout=5)
        launcher_log.close()
        proxy.stopping = True
        time.sleep(0.6)
        proxy.sock.close()

    events = proxy.events
    connects = [e for e in events if e["kind"] == "connect"]
    accepted = [e for e in connects if e.get("handshake") == "COMPLETED"]
    hosts = sorted({e["host"] for e in connects})
    service_log = probe_root / "rootfs/data/service.log"
    log_lines = service_log.read_text(errors="replace").splitlines() if service_log.exists() else []
    # Keep only lines about transport/TLS/bag/policy: the debug log of the Apple libraries is verbose and
    # may carry device identifiers; the full file stays in the throwaway runtime (deleted unless --keep).
    import re  # noqa: PLC0415
    interesting = re.compile(r"tls policy|curl|SSL|TLS|certif|verif|bag|http(s)?://[a-z0-9.-]*apple\.com|"
                             r"status=|HTTP/|response code|failed|error|listening|dns:", re.I)
    log_tail = [line for line in log_lines if interesting.search(line)][-80:]
    report.update(events=events, hosts_seen=hosts, connects=len(connects), handshakes_completed=len(accepted),
                  plaintext_bytes=sum(e.get("plaintext_bytes", 0) for e in accepted),
                  service_log_lines=len(log_lines), service_log_filtered=log_tail,
                  finished=time.strftime("%Y-%m-%dT%H:%M:%S%z"))
    if args.direct:
        if connects or any(e["kind"] == "plain_http" for e in events):
            verdict = "FAIL: traffic reached the idle probe proxy in direct mode"
        else:
            verdict = "INFO: direct mode; judge the filtered service log (expect verified Apple requests to succeed)"
    elif not connects:
        verdict = "INCONCLUSIVE: the wrapper sent no HTTPS request through the proxy"
    elif accepted:
        verdict = "FAIL: the wrapper completed a TLS handshake with an untrusted certificate"
    else:
        verdict = "PASS: every TLS handshake with the untrusted certificate was aborted by the wrapper"
    report["verdict"] = verdict
    out = wrapperctl.BUILD / f"probe-tls-{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}.json"
    out.write_text(json.dumps(report, indent=2) + "\n")
    os.chmod(out, 0o600)
    shutil.rmtree(certdir, ignore_errors=True)
    if not args.keep:
        shutil.rmtree(probe_root, ignore_errors=True)
    print(json.dumps({"mode": report["mode"], "verdict": verdict, "hosts_seen": hosts, "connects": len(connects),
                      "handshakes_completed": len(accepted), "service_up": report["service_up"],
                      "report": str(out)}, indent=2))
    return 0 if verdict.startswith(("PASS", "INFO")) else 1


if __name__ == "__main__":
    sys.exit(main())
