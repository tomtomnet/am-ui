# Lossless (ALAC) route: native FairPlay through the local wrapper

Status 2026-11-08, afternoon: **working end to end and integrated.** `tools/lossless/login.sh` was run
at 11:56 UTC; Apple's library completed the password exchange, the trusted device showed the two-factor prompt,
the six-digit code was typed into the wrapper and the login succeeded (`login successful`, `token cache
saved`). The first rip (song A, `audio-alac-stereo-48000-24`) took 0.9 s and probes as ALAC
24-bit/48 kHz, 2 channels, decoding cleanly. am-ui now has `amui.lossless` (variant `"lossless"`,
cache key `<id>.lossless`), the player kind `lossless` with AAC fallback, a Lossless switch in the Sound
popover and the `Lossless` / `Hi-Res Lossless` badge; sections 7 and 10 below record what was built.

Update 2026-11-12: this sign-in is now **the app's default sign-in** (section 5a). Connect opens a
sheet in the app that drives the same wrapper login over a pipe, `./setup.sh` builds the route by
default, and the engine uses the music user token the login leaves in `var/wrapper` for Apple's web
API as well, so one sign-in covers the library, Dolby Atmos and lossless.

Correction to section 2: the "missing trusted-device push" analysis did **not** hold for this account on
this day. The native `auth/v1/native` flow run by the wrapper's Apple libraries did produce a code
prompt on the trusted device without any extra request (proposals A and B were never needed and remain
unimplemented). The static finding (no `idmsa`/`trusteddevice` strings in the libraries) stands; the
push must therefore be triggered server-side by Apple's own `auth/v1/native` handling. Earlier failures
with a QEMU-based wrapper tried before are unexplained and may have been TLS or DNS related (both
repaired here).

Everything lives under `tools/lossless/` (scripts, patches, the Go primitive's source),
`var/build/lossless/` (build tree, git-ignored) and `var/wrapper/` (private runtime, 0700, git-ignored).

Why this route exists: the proven am-ui path (saved web session + Widevine license) covers Dolby Atmos
(E-AC-3 JOC) and AAC, but Apple refuses the Widevine license for the ALAC rendition (status `-1002` in
the hand-over). ALAC is only served through FairPlay to a client that proves it is an Apple Music
device, and the only such client that runs on Linux is Apple's own Android library stack.

## 1. Decision: the TLS-repaired wrapper-lite, with two more am-ui patches

Chosen: **WorldObservationLog/wrapper, branch `lite`, commit `386ec82`**, as reviewed on 2026-11-05
(`experiment/tls-defaults` worktree in the reference workspace = patch 0001), plus am-ui patches 0002
(host DNS) and 0003 (TLS verification inside Apple's libcurl, see section 3). It runs rootless in a
user namespace through the upstream `wrapper-lite-rootless` launcher: **no QEMU, no root, no system
packages**. The companion downloader (zhaarey/apple-music-downloader 0.1.4, `452ba0b`) is built against it.

Not chosen: **Adam / silentone12725** (`adam-src`, `dea30e5`). Reasons, in order of weight:

1. On the one question that decides the sign-in problem, the two are identical. Adam's `drm/native/main.c`
   is a vendored copy of the same wrapper code driving the same opaque `storeservicescore::AuthenticateFlow`
   with the same Apple libraries (every `.so` in Adam's `drm/fairplay-libs.lock` matches the bytes in the
   wrapper's rootfs, 22 of 22 SHA-256; `tools/lossless/fairplay-libs.sha256`). Neither issues a
   trusted-device push request (section 2).
2. Adam's loader is a different, unaudited binary path: a prebuilt `libhybris-core.so` (a Git LFS pointer
   in the clone, not fetched) embedded into `libdrm_client.so` and loaded from a memfd, plus a Go engine
   designed around Electron's SSE challenge flow. The wrapper's launcher is 150 lines of C we read
   (`wrapper-lite-rootless.c`) and Android's own `linker64` in a chroot.
3. The wrapper already had a tested TLS repair, a reviewed helper and a probe from the earlier session;
   the downloader speaks its HTTP API and is built. Adam would have meant a second full review.
4. The same author's **musickit-sdk-linux** (HEAD `f8b4dc8`, cloned into the scratchpad for inspection)
   has dropped the Android libraries entirely ("switch to the Widevine key-context backend"; `docs/building.md`:
   "No proprietary or Android libraries are involved: content keys come from Widevine"). It claims ALAC keys
   via `acquireWebPlaybackLicense` with a user-supplied Widevine device identity (`sdk/aacstream/aacstream.go:209-243`,
   `docs/security.md`). That contradicts our `-1002` observation; the difference may be the Widevine
   device's security level. It is an unverified lead, not a sign-in route, and it offers nothing for 2FA.

Adam does contain two sign-in robustness ideas worth copying later: it cancels Apple's credential request
when no sign-in is in progress instead of resubmitting (`main.c:343-367`; lite submits an empty
username/password, section 3), and it pauses sign-ins after two rejections because Apple locks accounts
(`engine/core/drm/signin_failure.go:69-144`).

## 2. The sign-in flow and the missing trusted-device push

### Call sites (exact)

wrapper-lite (`var/build/lossless/wrapper-src/lite/`, identical lines in the reference worktree):

| step | where |
|---|---|
| login entry: fork, `install_hooks()`, `init_ctx()`, `login()`, `cache_login_tokens()` | `lite_main.cpp:540-600` |
| `login()`: `AuthenticateFlow::run()`, response type 6 = success, else `customerMessage`/`errorCode` logged | `auth.cpp:133-164` (run at `:143`) |
| Apple's library calls back for credentials; `requiresHSA2VerificationCode()` read | `auth.cpp:49-57` |
| prompt `2FA code:`, `scanf("%6s")`, code appended to the password | `auth.cpp:61-75` (append at `:70`) |
| submit: `CredentialsResponse` with username, password(+code), response type 2 (OK button) | `auth.cpp:108-123` |

Adam (`adam-src/drm/native/main.c`): `credentialHandler` `:330-424` (library mode asks the Go engine for
the code through `g_drm_auth_cb("2fa", …)` at `:355-367`, cancels when none arrives), `login()` `:573-640`.
Go side: `engine/core/drm/auth.go:111-185` blocks until `POST /api/v1/drm/challenge`; `backend.go:163-166`
defines a `ChallengeDeviceApproval` type that nothing ever emits (`adamplay_backend.go:861` handles only
`"2fa"`). musickit-sdk-linux: no native sign-in component at all (section 1).

### What the network side is (observed, logged out, no credentials)

The direct probe (`tools/lossless/probe-tls.py --direct --debug-log`, report
`var/build/lossless/probe-tls-20261108T024626Z.json`) shows the library's own HTTP log: the URL bag
`https://init.itunes.apple.com/bag.xml?ix=6&locale=en-US`, `https://xp.apple.com/register`, the playback
dispatcher `https://play.itunes.apple.com/WebObjects/MZPlay.woa/wa/subPlaybackDispatch` and the
authentication endpoint **`https://auth.itunes.apple.com/auth/v1/native`**. The message it posts is the
one the 2026-11-05 static analysis decoded from `libstoreservicescore.so` (`appleId`, `attempt`,
`password`, `rmp=0`, `why=signIn`, `guid`, `createSession`), and it is the same protocol ipatool uses
(`pkg/appstore/appstore_login.go:411-437`; clone `d9c0247` inspected): attempt 1, Apple answers
`failureType -5000`; attempt 2 with the same password; if the password is right Apple answers
`customerMessage = MZFinance.BadLogin.Configurator_message` = "code required"; the client re-posts with
`password + six digits`. In the wrapper the library runs the attempts and the callback appends the code.

Two 2026 facts frame the symptom (prompt appears, no code arrives on the trusted device):

* **Web flow:** rclone #9359 / PR #9234 (`d0c469c3`, saved under
  `reports/wrapper-security-20261105/auth-static/rclone-reference/`): since iOS 26.4 the SRP 409 no longer
  pushes automatically; the client must `PUT https://idmsa.apple.com/appleauth/auth/verify/trusteddevice/securitycode`
  with the session headers (`scnt`, `X-Apple-ID-Session-Id`) to make trusted devices show a code.
* **Native store flow:** no public client has a code-request step. ipatool #132 (comment 2026-07-11,
  macOS 26.5.1): "no 2FA being generated by any of my authenticated MacBooks … Tahoe doesn't have a Get
  Verification Code option" is the same symptom on the same endpoint. ipatool #642 (open, 2026-11-06..08)
  additionally records that Apple retired the legacy `MZFinance … /authenticate` endpoint this week and
  that `auth/v1/native` requires a FairPlay SAP action signature **and** Store AMD device-proof headers
  (`X-Apple-AMD`, `X-Apple-AMD-M`, Anisette type 2); one commenter resolved it exactly as this route does,
  by "letting the Apple Music native authentication component complete device authentication". The
  wrapper's `libCoreFP`/`libCoreADI` produce those proofs (the `adi.pb` file in the runtime is the ADI
  state), and our probe saw `auth/v1/native` answer HTTP 200 with a store error rather than 204/404, so the
  wrapper is compatible with Apple's server behaviour as of 2026-11-08.

Static evidence from 2026-11-05 stands: the three Apple libraries contain no `idmsa`/`trusteddevice`/
`securitycode` strings, and `requiresHSA2VerificationCode` is a flag in Apple's response, not a delivery
acknowledgement. **Conclusion: neither implementation can request the push, and the native library has no
primitive for it.**

### Proposal (UNTESTED; do not treat as a fix until tried once)

Apple's six-digit codes are generated on the trusted device and are account-scoped: a code produced for
one pending sign-in is accepted by another one of the same account within its validity window (that is
why Settings' "Get Verification Code" ever worked with ipatool/the wrapper; Apple support 102606). The
2026 web flow still pushes a code when a client asks for it. Hence:

**A. Try first, no code change.** Start `tools/lossless/login.sh`. When the wrapper prints `2FA code:`,
in a browser sign in to <https://account.apple.com> (or icloud.com) with the same Apple account. Apple's
own web sign-in issues the trusted-device request, the trusted device shows "Allow" and a six-digit code; type
that code into the wrapper prompt (it waits indefinitely). If Apple rejects it, the attempt simply fails
(`auth error …`); do not retype the password more than twice in a row (lockout).

**B. Scripted equivalent (minimal patch, outside the opaque library).** A `tools/lossless/push-2fa.py`
run by `login.sh` after the password prompt, replaying rclone's sequence against
`https://idmsa.apple.com/appleauth/auth` with Python's `ssl` (host CA store):

1. `GET /authorize/signin?frame_id=auth-<uuid>&language=en_US&skVersion=7&iframeId=auth-<uuid>&client_id=<id>&redirect_uri=https://www.icloud.com&response_type=code&response_mode=web_message&state=auth-<uuid>&authVersion=latest`; record `scnt`, `X-Apple-ID-Session-Id`, `X-Apple-Auth-Attributes` from every response.
2. `POST /federate?isRememberMeEnabled=true` `{"accountName": email, "rememberMe": true}`.
3. `POST /signin/init` `{"a": b64(A), "accountName": email, "protocols": ["s2k","s2k_fo"]}` → `{iteration, salt, protocol, b, c}`.
4. Key: `h = SHA256(password)`; `s2k`: PBKDF2-HMAC-SHA256(h, salt, iteration, 32); `s2k_fo`: the same over `hex(h)`.
   SRP-6a, RFC 5054 2048-bit group, g = 2, SHA-256: `k = H(N | pad(g))`, `x = H(salt | H(":" | key))` (no username),
   `u = H(pad(A) | pad(B))`, `S = (B − k·g^x)^(a + u·x) mod N`, `K = H(S)`,
   `M1 = H((H(N) xor H(pad(g))) | H(email) | salt | pad(A) | pad(B) | K)`, `M2 = H(pad(A) | M1 | K)`
   (rclone `backend/iclouddrive/api/srp.go` at `d0c469c3`).
5. `POST /signin/complete?isRememberMeEnabled=true` `{"accountName", "m1", "m2", "c", "rememberMe": true, "trustTokens": []}` → **409 = second factor required**.
6. `PUT /verify/trusteddevice/securitycode` (empty body) with the same headers → the trusted device shows the code.
   Headers on 2–6: `Accept`/`Content-Type: application/json`, `Origin`/`Referer: https://idmsa.apple.com`,
   `X-Apple-Widget-Key` and `X-Apple-OAuth-Client-Id` = the public iCloud web client id rclone used
   (`d39ba9916b7251055b22c7f910e2ea796ee65e98b2ddecea8f5dde8d9d1a815d`), `X-Apple-OAuth-Client-Type: firstPartyAuth`,
   `X-Apple-OAuth-Redirect-URI: https://www.icloud.com`, `X-Apple-OAuth-Require-Grant-Code: true`,
   `X-Apple-OAuth-Response-Mode: web_message`, `X-Apple-OAuth-Response-Type: code`, `X-Apple-OAuth-State`/`X-Apple-Frame-Id: auth-<uuid>`,
   `X-Apple-I-Require-UE: true`, `X-Apple-I-FD-Client-Info`, the Safari UA, plus `scnt`/`X-Apple-ID-Session-Id` once known.
   The code is then typed into the wrapper's prompt; the idmsa session is abandoned (nothing else is needed).

Costs of B: the password is sent to a second Apple endpoint (idmsa, the one Safari uses), a wrong password
counts twice toward Apple's lockout, and about 150 lines of new crypto code. Only write it if A works
manually and the user wants it scripted. Neither A nor B has been tried; both rest on the account-scoped
code assumption above.

## 3. Security review

### Verified (by reading source, by hashing, or by running the credential-free probes)

* **Binds 127.0.0.1 only**: `lite_main.cpp:35` default host, passed explicitly by the helper; `ss` showed
  `127.0.0.1:12351` only. No `--cors`, so the API has no cross-origin exposure either.
* **TLS, wrapper's own curl path** (dev-token scrape, lyrics, webPlayback, license): verification and CA
  bundle forced in `CurlEasy` and the five per-call bypasses removed (patch 0001, 2026-11-05, six
  Android-libcurl tests then). Re-verified today: MITM probe rejects with `TLSV1_ALERT_UNKNOWN_CA`.
* **TLS, Apple's own libraries (new finding and fix).** The first MITM probe run
  (`probe-tls-20261108T023703Z.json`) caught the library's bag fetch to `init.itunes.apple.com` completing
  three TLS 1.3 handshakes against a self-signed certificate and sending 213 bytes of HTTP each time. The
  debug log shows why: `MediaPlatform::HTTPRequest_Curl` sets `CURLOPT_SSL_VERIFYPEER=0` and
  `CURLOPT_SSL_VERIFYHOST=0` on every request, and the rootfs has no Android CA store anyway. Patch 0003
  turns the upstream debug-only `curl_easy_setopt` hook into a mandatory policy: every handle gets
  `VERIFYPEER=1`, `VERIFYHOST=2`, `CAINFO=/system/etc/workspace-ca-bundle.pem` right before
  `curl_easy_perform`/`curl_multi_add_handle`, downgrades through `curl_easy_setopt` are refused,
  `CURLOPT_CAPATH` is dropped, pinned public keys the library sets are kept, and the process exits if the
  hooks cannot be installed. Result: MITM probe `PASS` (all handshakes aborted,
  `probe-tls-20261108T024441Z.json`, `…024947Z.json`), and in direct mode the verified requests to
  `init.itunes.apple.com`, `xp.apple.com`, `play.itunes.apple.com` and `auth.itunes.apple.com` all return
  HTTP 200 (`…024626Z.json`). All twelve Apple hosts checked with `openssl s_client` chain to DigiCert
  Global Root G2 and verify against the host bundle. The password exchange runs through this stack
  (`login()` is called after `install_hooks()`), so it is now verified too. This is exactly the limit the
  Nov 5 review flagged as unknown ("the initial password exchange … uses the opaque implementation").
* **No token/password logging** in the wrapper source (patch 0001 removed the prefixes; the credential
  handler logs title/message/2FA flag only; `cred submit` lengths are debug-level). The helper never prints
  token contents; `status.sh` reports file presence and sizes only.
* **DNS**: upstream hard-codes AliDNS 223.5.5.5/223.6.6.6 inside the chroot (`bridge.cpp:65`). Patch 0002
  passes the host's resolvers (`LITE_DNS_SERVERS`, from `/etc/resolv.conf` → `127.0.0.53`); the log confirms
  `dns: 1 resolver(s) from LITE_DNS_SERVERS` and the dev-token scrape succeeds through it.
* **Rootless**: `unshare(CLONE_NEWUSER|NEWNS|NEWPID)` + uid/gid map + chroot (`wrapper-lite-rootless.c`),
  no setuid, no sudo; confirmed with `unshare -Ur`.
* **Library provenance**: the 22 Apple/ICU/bionic libraries match the SHA-256 values Adam pins
  independently from two upstream commits (`fairplay-libs.sha256`); `stage` refuses to copy a runtime
  whose libraries differ. `lite` itself is built here from source (`manifest.json` has the hashes).
* **Private storage**: `var/wrapper` 0700, account files 0600 (the helper re-tightens after every run;
  Apple's `adi.pb` is created 0664 otherwise). The password goes only into a 0600 argument file inside the
  runtime that the helper creates exclusively and removes when the login process exits (upstream
  `LITE_ARGS_FILE` mechanism; nothing in argv, nothing in the shell history).
* **Clean start/stop**: `start.sh` → `/status` answers in about 1 s; `stop.sh` sends SIGTERM to the process
  group, the wrapper logs `received signal 15 … wrapper-lite stopped`, the helper waits for its own pid and
  descendants only (recorded pid + start time), and an immediate restart works.
* **Downloader**: Go `-mod=readonly`, `GOPROXY=off`, standard TLS (no `InsecureSkipVerify`, Nov 5 review);
  its only non-Apple network use is the GitHub release-update notice at startup (not disableable in config).

### Assumed or still opaque

* The Apple libraries are closed binaries (Apple Music for Android, old build). Their behaviour after login
  (what the account database holds, what `xp.apple.com/register` reports: it is Apple's metrics endpoint,
  contacted at every start) is not audited; the probe covered only the logged-out startup and one playback
  dispatch. "Telemetry-free" cannot be claimed.
* Verification now depends on `curl_easy_setopt` being the only way the library configures TLS. A
  `CURLOPT_SSL_CTX_FUNCTION` callback could in principle override it; none was observed (the MITM probe
  would show a completed handshake), but that is a runtime observation, not proof.
* The wrapper, logged out, answers Apple's credential request with an **empty** username/password when
  something triggers playback (`auth.cpp:114-117`); the probe's optional `/m3u8` call made Apple return
  `-5000`. Harmless, but a logged-in session whose token expired would do the same; Adam cancels instead.
* The 2FA prompt is a flag in Apple's response; nothing proves a push was requested or delivered.
* Debug logging (`--log-level debug`) prints the libraries' internal logs including device identifiers;
  the probe uses it only on a throwaway runtime without account data. Never enable it on `var/wrapper`.

## 4. Build (done; reproducible from pinned inputs)

`tools/lossless/build.sh` has two input modes and writes under `var/build/lossless/`. On a machine
without the reference workspace (**fetch**, added 2026-11-12; `./setup.sh --lossless`) it fetches the
wrapper (386ec82), cJSON (c859b25 = v1.7.19), Dobby (9d27180, the commit the reviewed build resolved
`master` to) and the downloader (452ba0b) by commit id, downloads the Android NDK r23b and, when no
suitable Go is installed, Go 1.27.3 (both checked against SHA-256 values in the script), applies
patches 0001 to 0003 to the pristine wrapper and lets Go fetch the modules through the module proxy
(`-mod=readonly`, verified against the downloader's `go.sum`). The fetched wrapper, cJSON and Dobby
trees are byte-identical to the sources of the reviewed build. With the reference workspace present
(**reference**, the original offline mode) it reads only that workspace (`~/Documents/apple-music-gpu`,
or `AMUI_WORKSPACE`):

1. copies the TLS-repaired worktree (`builds/music-downloader/wrapper-tls-src`, 386ec82 + patch 0001,
   verified by three greps) without its `.git` link, applies `patches/0002-dns-from-env.patch` and
   `patches/0003-native-curl-tls-policy.patch`;
2. `cmake … -G Ninja -DCMAKE_BUILD_TYPE=Release -DBUILD_HOST_LAUNCHERS=OFF -DFETCHCONTENT_FULLY_DISCONNECTED=ON`
   with the workspace NDK r23b and the cJSON/Dobby sources copied from the Nov 5 build (`_deps/`);
   output `wrapper-src/rootfs/system/bin/lite` (sha256 `6782ddca…`). The six Android libraries `lite`
   links (`androidappmusic`, `storeservicescore`, `mediaplatform`, `c++_shared`, `curl`, `log`) are
   passed as `-D<NAME>_LIB:FILEPATH=<wrapper-src>/rootfs/system/lib64/lib….so`, see below;
3. `cc -O2 -Wall` the rootless launcher → `bin/wrapper-lite-rootless` (`c89bae68…`);
4. copies `downloader-src` (0.1.4, `452ba0b`) and the Go module cache, builds with the workspace Go 1.27.3
   (`GOPROXY=off -mod=readonly CGO_ENABLED=0`) → `bin/amdl` (`ce9d3fda…`), runs `--version`;
5. `wrapperctl.py stage`: creates `var/wrapper/rootfs/system/{bin,lib64,etc}` from the build tree, verifies
   the 22 pinned hashes, copies the host CA bundle (`/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem`) to
   `rootfs/system/etc/workspace-ca-bundle.pem`, writes `var/wrapper/manifest.json`; `rootfs/data` (account
   state) is created if missing and otherwise left alone;
6. renders `tools/lossless/amdl-config.yaml.in` → `var/wrapper/amdl/config.yaml` (kept if present).

Re-running is idempotent (about 2 s when nothing changed). Build logs: `var/build/lossless/logs/`.

**Which libraries `lite` is linked against (fixed 2026-11-12).** Upstream's `CMakeLists.txt` looks the
rootfs libraries up with `find_library(CURL_SHARED_LIB curl PATHS <rootfs>/system/lib64)` (the same
for the other five). `PATHS` is searched after CMake's default folders, and the project sets the NDK
compilers itself instead of using a toolchain file, so those defaults are the build machine's own
library folders. On a machine with the development files of a library of the same name (libcurl:
`/usr/lib…/libcurl.so`) CMake therefore chose the system's glibc library: `lite` then asked the
Android loader for `libcurl.so.4`, which the rootfs does not have, and neither the sign-in nor the
service could start (`CANNOT LINK EXECUTABLE: could not load library "libcurl.so.4"`). A machine
without those development files built correctly, which is why the reviewed build never showed it.
`build.sh` now names all six libraries to CMake (a `-D` also replaces the wrong answer kept in an
existing build folder's cache) and, after the build, reads `lite`'s NEEDED entries with the NDK's
`llvm-readelf` and stops unless every one is a file of the rootfs (`libdl.so` excepted: the loader
provides it). Reproduced and checked in a scratch build with a `libcurl.so` put on CMake's search
path (`CMAKE_LIBRARY_PATH`): before, NEEDED `libcurl.so.4` and the start check failing with the
message above; after, NEEDED `libcurl.so` and the start check passing, also over a build folder
whose cache still held the system library. `wrapperctl.py selftest` names such a build ("built
against this system's libcurl.so.4 …; `./setup.sh` builds it again"); the change to `build.sh`
makes every earlier build `stale`, so `./setup.sh` rebuilds it.
Requirements: cmake, ninja, a C and C++ compiler, python3, patch, tar, git (fetch mode also curl and unzip); x86_64 only.

Layout of the private runtime (`var/wrapper/`, 0700):

```
rootfs/system/bin/{linker64,lite}   rootfs/system/lib64/*.so (98)   rootfs/system/etc/workspace-ca-bundle.pem
rootfs/data/                        account state: mpl_db/*.sqlitedb, adi.pb, DEV_TOKEN, MUSIC_TOKEN,
                                    STOREFRONT_ID, token_cache.json, service.log, (.login-args during login)
run/service.pid.json  run/helper.lock   logs/launcher.log   amdl/config.yaml   amdl/out/{alac,…}   manifest.json
```

## 5. Service and login (what the user does)

All scripts are thin wrappers around `tools/lossless/wrapperctl.py`; the port is `127.0.0.1:12351`
(`LOSSLESS_PORT` to change; the downloader config follows it on the next `build.sh`).

```sh
cd ~/Documents/am-ui
tools/lossless/status.sh            # running? storefronts? signed in? (exit 0 when the service answers)
tools/lossless/login.sh             # the Apple account sign-in in a terminal (the app's Connect does the same in a sheet)
tools/lossless/start.sh             # detached service; /status answers within ~1 s
tools/lossless/stop.sh              # stops only what start.sh started
tools/lossless/login.sh --explain-only   # prints the notice without asking anything
```

`login.sh` prints the notice reproduced by `--explain-only` (what runs, what is asked, where state goes),
asks for the email (visible) and the password (hidden), checks the wrapper's limits (password at most
57 bytes because password + code share a 63-byte buffer, no `:` in the email, no trailing whitespace or
line breaks in the password), writes the 0600 argument file and hands the terminal to the wrapper. Expected
wrapper output, in order: `tls policy: certificate verification enforced …`, `dns: …`, `initializing ctx…`,
`logging in...`, `dialogHandler: {title: Sign In …}`, `credentialHandler: {… 2FA: false}`, then
`credentialHandler: {… 2FA: true}` and the prompt **`2FA code:`**. Type six digits, Enter. Success:
`login successful`, `login complete, exiting`, then the helper's line "Login completed: fresh account token
files exist". Failure: `server message: …` / `auth error: code=…` and the helper's "login did not produce
fresh account tokens"; an earlier sign-in is put back in that case (section 5a). If no code arrives: Ctrl+C
(see section 2, proposal A, before retrying; do not retype the password repeatedly).

Confirming success without secrets: `tools/lossless/start.sh && tools/lossless/status.sh` must show
`"signed_in": true` and a non-empty `"storefronts"` (a numeric Apple storefront id such as `143444`; not a
secret), and `account_files` all `true`. Nothing else needs to be shown or pasted. First test of the whole
chain (only after that):

```sh
var/build/lossless/bin/amdl -c var/wrapper/amdl/config.yaml --select 'https://music.apple.com/<sf>/album/<slug>/<albumId>'
ffprobe -hide_banner var/wrapper/amdl/out/alac/<albumId>/<songId>.m4a     # expect: Audio: alac, 44100/48000/96000 Hz, stereo, s16p/s32p (24 bit)
```

Signing out: `tools/lossless/stop.sh && rm -rf var/wrapper/rootfs/data` (removes the account database and
tokens; the next start recreates an empty one). Note that Apple will list this "device" in the account's
device list after a successful login; that is expected for the native flow.

## 5a. The app's sign-in (2026-11-12)

The wrapper login above is what the Connect button runs. The pieces:

* **One credential, two sources.** The login leaves the account's music user token in
  `var/wrapper/rootfs/data/MUSIC_TOKEN`. Apple's web API accepts it where it accepts the browser
  session's cookie: checked read-only on 2026-11-12 for the storefront, the library, recently played,
  catalog search, `/v1/play/assets`, and the Widevine licenses of the Dolby Atmos and the AAC stream
  (both decrypted). Write requests (add to library, playlist edits) were not tried with it. `amui.web`
  reads the file per request, uses it first (`AMUI_SIGNIN=browser` reverses the order) and, on a
  401/403, repeats the request once with the other sign-in; the one Apple refused is set aside until
  the next sign-in.
* **`wrapperctl.py login --pipe [--stop-service]`**: the same login without a terminal. Standard
  input takes JSON lines (`{"username","password"}`, `{"code"}`, `{"cancel": true}`), standard output
  reports `phase` events (`preparing`, `signing_in`, `code_required`, `verifying`, `finishing`),
  `notice` and one `result` (`ok`, or a `kind`: `input unavailable busy credentials code apple
  cancelled timeout failed`, with the `steps` the attempt went through and, after a failure, the
  `log` path). The password travels pipe → 0600 argument file → wrapper, the code pipe → the
  wrapper's standard input; neither reaches a command line. `amui.session.android_signin()` drives
  it for the sheet; `amui.session.android_signin_terminal()` runs the terminal login for
  `./am-ui cli connect`.
* **What a failed attempt leaves behind.** The wrapper's output is parsed in memory; its last 200
  lines are written to `var/wrapper/logs/signin-last.log` (0600) when an attempt fails, and the
  file is removed by the next success. Scrubbing is done per line before anything is kept: a line
  containing the typed password or code is dropped whole (replacing the text in place would show
  where a password coincides with fixed log text), the Apple ID and any e-mail address become
  `<apple id>`, runs of 32 or more token characters `<left out>`, runs of 8 or more digits
  `<number>`. The result's message is built from what the wrapper reported: Apple's own message or
  dialog text, an error number, "response type N" when Apple gave no reason, the token step that
  failed after an accepted sign-in, a TLS refusal, or that Apple never answered.
* **Start check.** `wrapperctl.py selftest` runs the launcher with `--help` in the staged runtime:
  the wrapper prints its usage text before it initialises anything, so the check needs no account
  and no network and leaves the account data alone. It fails where the launcher cannot make its
  user, mount and PID namespaces (the message names the refused step and, when one is set, the
  kernel setting in the way), where the runtime's folder cannot be prepared, or where the program
  cannot be loaded (a build linked against a library of the build machine is named as such, see
  section 4). `login` runs it before a password is read (result kind `unavailable`), `start`
  uses it to explain a service that does not answer, and `./setup.sh` and the doctor report it.
* **The launcher's name.** The launcher passes its own argument list on to the wrapper, and the
  wrapper ends without output when `argv[0]` is 128 characters or longer (measured: 127 works, 128
  does not; the length of the other arguments and of the environment does not matter). Started by
  its full path, an am-ui folder deep in the file system therefore broke the route silently. The
  helper starts it as `wrapper-lite-rootless` with the path given separately, and recognises
  running launchers and wrappers by their program file (`/proc/PID/exe`) instead of the command line.
* **Attempt limit** (the lockout risk of section 8). Two guards, either one sufficient:
  `patches/0004-answer-credentials-once.patch` makes the wrapper end the login when Apple's library
  asks for the password or the code a second time (upstream answers again with the same values,
  which counts as another wrong attempt), and the pipe driver never writes a second answer. A
  refused password or code therefore ends the attempt; the person starts a new one. Because the
  driver's own check comes after an unpatched wrapper has already answered again, `login` (terminal
  and pipe) refuses to run a staged wrapper that lacks the patch (result kind `unavailable`, with
  the update steps).
* **An earlier sign-in survives a failed attempt.** The wrapper deletes `MUSIC_TOKEN` and
  `STOREFRONT_ID` at the start of every login. The helper moves them aside first
  (`*.before-login`) and puts them back unless the attempt produced fresh ones; `start` repairs
  leftovers of an interrupted attempt.
* **The service is stopped for a login** (it holds the account database) and started again by the
  next lossless song. The helper holds the route's lock for the whole attempt, so a second login
  or a `start` cannot run into it.
* **Build state.** `wrapperctl.py build-state` prints `absent | stale | unstaged | current` and
  `running | stopped`; `stale` compares the manifest's `source_fingerprint` (a hash over `build.sh`,
  the pin file, the Go helper and the patches) with the checkout. `./setup.sh` builds on anything
  but `current`, and leaves a build alone while its service is running (it says how to update).

Not verified here: a real sign-in through the sheet (the stand-in wrapper of `tests/test_signin.py`
plays Apple's side in the tests; the one real login so far was the terminal one of 2026-11-08).

## 6. Key and decrypt protocol (for a later `amui.acquire` variant "lossless")

HTTP on `127.0.0.1:12351`, JSON envelope `{"code": 0, "msg": "SUCCESS", "data": {…}}`; errors come back as
HTTP 200 with `code` 400/404/500 and `msg` (`lite_main.cpp`, `internal/wrapper/*.go`).

| endpoint | purpose | notes |
|---|---|---|
| `GET /status` | `{"regions": ["<storefront>"]}`; empty list = logged out | the health check |
| `GET /m3u8?adamId=<song id>` | `{"m3u8": "<URL of the account's device master playlist>"}` | library call `SVPlaybackLeaseManager::requestAsset(adam, ["HLS"])` (`playback.cpp:48-82`) = `subPlaybackDispatch`; falls back to the `subDownload` purchase asset (`:13-46`). Serialized by a mutex. Needs login (otherwise `404 failed to get m3u8`). **For a song without a lossless rendition the URL is not a playlist** but the song's AAC media file (`streamingaudio.itunes.apple.com/…/mzaf_<n>.aac.a.m4p`, about 12 MB): `device_master()` answers `NoLosslessError` for a URL whose path does not end in `.m3u8` without fetching it, and for a `.m3u8` that is not text (`web.NotText`), so the player plays the AAC version (fixed 2026-11-12: the file used to be read as a playlist and the song stopped with "Manifest was not UTF-8 text"). The answer takes about 0.3 s either way |
| `GET /key?adamId=<song id>&uri=<skd URI>` | the decryption **template**: `{"adamId","keyUri","contentKey","ctx": b64(32768 B), "state": b64(8448 B), "rcx","rax","rdx","r9","rbp": "0x…"}` | one or two FairPlay license round trips to `https://play.itunes.apple.com/WebObjects/MZPlay.woa/music/fps` (`playback.cpp:103-123`, `288-309`), then a Dobby hook captures the decryptor's round-1 state inside `libCoreLSKD.so` (`:125-172`). `adamId=0` is only valid with the prefetch URI below |
| `GET /lyrics`, `GET /webplayback`, `POST /license` | web-API passthroughs with the account token (Widevine/PlayReady licenses for the AAC/Atmos route) | not needed for ALAC |

Acquisition sequence as the downloader does it (`internal/app/rip.go:63-85`, `media.go:399-620`,
`internal/fairplay-rip/runv4.go`):

1. **Master playlist**: with `get-m3u8-mode: all` the device master from `/m3u8` replaces the web
   `enhancedHls` URL. It is a plain HTTPS CDN URL; the fetch needs no cookies or tokens.
2. **Variant**: `#EXT-X-STREAM-INF` entries with `CODECS="alac"`; the `AUDIO` group name ends in
   `-<sampleRate>-<bitDepth>` (e.g. `…-48000-24`, `…-192000-24`); the downloader sorts by average bandwidth
   and takes the first ALAC variant whose sample rate ≤ `alac-max` (`media.go:589-615`). Other codecs present:
   `mp4a.40.2` (AAC), `ec-3` + `atmos` group (Dolby Atmos), sometimes `ac-3`.
3. **Media playlist**: byte-range segments (`#EXT-X-BYTERANGE`) of **one** fMP4 file; non-byterange
   playlists are rejected (`runv4.go:101-103`). `#EXT-X-KEY` lines: keep only
   `KEYFORMAT="com.apple.streamingkeydelivery"` (`filterResponse`, `:404-428`); the URIs are `skd://…`.
   The first key is often the shared prefetch key `skd://itunes.apple.com/P000000000/s1/e1` (adamId `0`;
   the downloader embeds its constant template, `internal/wrapper/prefetch.go`), followed by song keys.
4. **Decrypt** (`DecryptFragment`, `runv4.go:~600-660`): scheme must be `cbcs`; for each `moof`+`mdat`
   fragment read `senc` subsample patterns, decrypt the protected bytes of every sample in whole 16-byte
   blocks with the pure-Go **Temari** port (`internal/temari`, MIT port of WorldObservationLog/Temari: a
   transliterated round chain driven by the template's `ctx` tables, 263 `state` slots and the five
   registers; `Template.DecryptInto`), remove `senc/saiz/saio/sbgp/sgpd(seig,seam)` and `pssh`, fix `trun`
   data offsets. The init segment is rewritten by `mp4ff`'s `DecryptInit` (`enca` → `alac`, `sinf` removed;
   one `stsd` entry). Keys may change at any segment; templates are cached per URI (`:311-328`).
5. **Output**: a plain fragmented MP4 written as `<song>.m4a`, then `defrag.DefragmentMP4` (one `mdat`),
   `alacfix` (adds the missing `TYPE_END` marker some Apple ALAC packets lack, otherwise FFmpeg errors),
   tags. Expected probe: `alac`, 2 channels, 16-bit/44.1 kHz for "Lossless", 24-bit at 48/88.2/96/176.4/192 kHz
   for "Hi-Res Lossless" (`alac-max` caps it; 192000 in the generated config, lower it to what the output
   chain can use). Sizes: roughly 25–35 MB for a 4-minute 24/96 track.

`amdl` flags of interest: `--select`, `--alac-max N`, `--debug` (prints every variant with codec/group/
bandwidth), `--lite-server URL`, `-c config`. Each `/key` is serialized in the wrapper and costs one or two
Apple round trips; `/m3u8` takes a playback lease for the account.

## 7. Integration plan for am-ui (done 2026-11-08; see section 10 for what was built)

1. **Decrypt primitive first, not a Python port.** Add a tiny Go program to the downloader module copy at
   build time (`cmd/amui-fairplay-rip/main.go`, source kept in `tools/lossless/`): it calls
   `fairplayrip.Run(adamId, playlistURL, outfile, cfg)` and `defrag.DefragmentMP4`, reading only
   `lite-server` from flags. That gives `amui.acquire` a subprocess with a stable contract
   (`--lite http://127.0.0.1:12351 --adam ID --playlist URL --out FILE`, exit 0 and a finished file) without
   any of the catalog/tagging logic. A Python Temari port (1200 lines of generated rounds) stays an option
   for later.
2. **`acquire(catalog_id, variant="lossless")`** in `amui/acquire.py`, mirroring `atmos`/`stereo`:
   `wrapperctl status`-style preflight (service answers and `regions` non-empty, else a clear
   `LosslessUnavailable("needs the native sign-in: tools/lossless/login.sh")`), `GET /m3u8`, parse the master
   (`amui.web.probe` already parses masters), pick the ALAC variant ≤ a configurable max sample rate
   (default 96000 or the output rate), run the primitive, verify with `ffprobe` (`alac`, rate, bit depth),
   store as `MEDIA_CACHE_DIR/<id>.lossless.m4a` with the usual meta JSON (`audio_group`, `codecs`,
   `bandwidth`, `sample_rate`, `bit_depth`). Never fall back to AAC silently.
3. **Decode and output**: FFmpeg's built-in `alac` decoder → 2-channel f32 (resample to the chain's rate
   with soxr if the file's rate differs) → the existing stereo path (FL/FR into the downmix sink). ALAC is
   stereo; the 9.1.6 chain is for Atmos.
4. **Service lifecycle**: start the wrapper on demand (`wrapperctl start` is idempotent, 1 s) and stop it
   when idle; it holds a playback lease for the account while running (section 8).
5. **UI**: a Sound-popover quality switch "Lossless" shown only when `status` reports signed in; settings
   entry pointing at `tools/lossless/login.sh` otherwise.
6. **Later**: 2FA push helper (section 2 B) only if A works; cancel-instead-of-resubmit and sign-in pause
   from Adam; a host-side block of `xp.apple.com` if the metrics registration is unwanted (needs a test
   that the library tolerates the failure).

## 10. What was built (2026-11-08)

* **`tools/lossless/go/amui-fairplay-rip/main.go`** (built by `build.sh` step 5b into
  `var/build/lossless/bin/amui-fairplay-rip`, hash in the build manifest): `--lite URL --adam ID --out FILE
  [--max-memory MiB]`, the rendition playlist URL on **stdin** (never in argv). Calls `fairplayrip.Run`,
  `fmp4unfrag.DefragmentMP4`, `alacfix.Fix`; umask 077; refuses non-HTTPS or non-Apple playlist hosts and a
  non-loopback `--lite`; redacts URLs from error messages; `done <bytes>` on success.
* **`amui/lossless.py`**: `configured()` (helper built + the three account files non-empty; this module never
  reads their contents, `amui.web` reads the token: section 5a), `available()` (+ 60 s back-off after a service failure), `ensure_service()` (idempotent start through
  `wrapperctl.py start`, remembers that it started it; `stop_service_if_started()` at player shutdown),
  `device_master()`, `alac_variants()`/`select_alac()` (highest rate ≤ `config.LOSSLESS_MAX_RATE`, default
  48000 because the chain runs at 48 kHz; else the lowest above), `rip()`, `acquire_lossless()` (phases
  service/manifest/download/verify/done; ffprobe must say `alac`, 2 ch, the rendition's rate),
  `quality_label()`, `status()`. Errors: `LosslessUnavailable` / `NoLosslessError` ⊂ `LosslessError` ⊂
  `AcquireError`. `NoLosslessError` covers both a master playlist without an ALAC rendition and a dispatch
  that names the song's AAC media file instead of a playlist (section 6, `/m3u8` row); it does not arm the
  back-off.
* **Engine**: `acquire.acquire(..., variant="lossless")` dispatches; `decode_stereo()` decodes ALAC too
  (soxr resampling when the source rate differs); `PcmCache.KINDS["lossless"]`; player kind `lossless`
  (`_stereo_kind()`: ALAC when the setting is on, the wrapper configured and the song not known to fail;
  else AAC), `set_lossless()` persisted, snapshot `lossless`/`lossless_available`, `format="lossless"`,
  `format_detail="Lossless · ALAC 24-bit/48 kHz"`; a lossless failure plays the AAC version with a
  notice and is remembered per song for the session.
* **UI**: `lossless`/`losslessAvailable` properties, `setLossless`, the Lossless row in the Sound popover
  (greyed out with the reason until configured), `FormatBadge` shows "Lossless"/"Hi-Res Lossless".
  CLI: `acquire --lossless`, `decode --lossless`, `lossless [--start|--stop|--check SONG]`.
* **Tests**: `tests/test_lossless.py` (18: selection, availability/back-off, the flow around a stand-in
  helper with a real tiny ALAC file, redaction, cancellation), player tests for the kind and its fallbacks.

## 8. Open risks

* **The code may still not arrive.** Nothing here changes Apple's push behaviour; proposals A/B are untested.
* **Account lockout**: Apple blocks sign-ins after a few wrong passwords. Since 2026-11-12 an attempt
  answers Apple once (section 5a); nothing limits how many attempts a person starts by hand.
* **Edits with the native sign-in's token** (add to library, playlist changes) are untested; a refusal
  falls back to the browser sign-in when one exists.
* **Apple-side churn**: the week of 2026-11-06 Apple retired a legacy auth endpoint (ipatool #642); the
  bundled libraries are an old Android build, and Apple can stop serving them at any time.
* **Playback lease**: the service requests a lease at every start (`setup_services`, `bridge.cpp`) and
  `/m3u8` dispatches playback; with one Apple Music stream per account this may interrupt music playing on
  another device or in the VM. Start the wrapper only while acquiring.
* **Unhandled dialogs**: lite's `dialogHandler` only answers "Sign In"; any other Apple dialog (concurrent
  stream, terms) is answered without a selected button, which Adam's notes say crashes the library.
* **Opaque libraries**: no audit of what the account database or `xp.apple.com/register` carries after login.
* **TLS policy scope**: enforced for libcurl in-process; a library-level `SSL_CTX` override would defeat it
  (none observed).
* **Downloader update check** contacts GitHub at every run with a generic UA; not configurable.

## 9. Files

`tools/lossless/`: `build.sh`, `start.sh`, `stop.sh`, `status.sh`, `login.sh`, `wrapperctl.py` (the
implementation), `probe-tls.py` (MITM and direct probes), `amdl-config.yaml.in`, `fairplay-libs.sha256`,
`patches/0001-tls-defaults-2026-11-05.patch` (the reviewed Nov 5 repair, for reference),
`patches/0002-dns-from-env.patch`, `patches/0003-native-curl-tls-policy.patch`,
`patches/0004-answer-credentials-once.patch`.
Private: `var/build/lossless/{bin,wrapper-src,wrapper-build,amdl-src,manifest.json,probe-tls-*.json,logs}`,
`var/wrapper/`.

References: the 2026-11-05 notes (`apple-music-gpu/notes/wrapper-security-2026-11-05.md`,
`wrapper-login-ios27-2026-11-05.md`, `adam-repo-review-2026-11-07.md`) and
`reports/wrapper-security-20261105/auth-static/`; rclone PR #9234 commit `d0c469c3`
(`backend/iclouddrive/api/{session,srp}.go`); ipatool `d9c0247` (`pkg/appstore/appstore_login.go`), issues
majd/ipatool#642 (2026-11-06..08) and #132 (comment 2026-07-11); WorldObservationLog/wrapper issues #37,
#58, #75; silentone12725/musickit-sdk-linux `f8b4dc8` (`docs/drm.md`, `docs/building.md`, `docs/security.md`);
Apple support article 102606.
