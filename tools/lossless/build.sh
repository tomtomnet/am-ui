#!/usr/bin/env bash
# Build the am-ui lossless (ALAC) route:
#   1. wrapper-lite (WorldObservationLog/wrapper lite 386ec82 + patches/0001 TLS repair, 0002 DNS from
#      the environment, 0003 native curl TLS policy, 0004 one answer per credential request) with the
#      Android NDK r23b
#      -> var/build/lossless/wrapper-src/rootfs/system/bin/lite
#   2. the rootless host launcher (user namespace, no QEMU, no root) -> var/build/lossless/bin/
#   3. apple-music-downloader 0.1.4 (452ba0b) -> var/build/lossless/bin/amdl, plus am-ui's
#      amui-fairplay-rip (the downloader's decrypt internals behind a small CLI; amui.lossless runs it)
#   4. the private runtime var/wrapper (0700): rootfs + host CA bundle; account data is never touched
#   5. var/wrapper/amdl/config.yaml for the downloader (kept if it exists)
#
# The inputs come from one of two places:
#   fetch      the pinned upstream commits from GitHub, the Android NDK r23b from dl.google.com and,
#              when no Go of the version in the downloader's go.mod is around, Go from go.dev. Every
#              input is checked against the commit ids and SHA-256 values below; Go modules come
#              through the module proxy and are checked against the downloader's go.sum. About 1 GB
#              of downloads and 3 GB on disk, all under var/build/lossless.
#   reference  an existing private workspace (AMUI_WORKSPACE or ~/Documents/apple-music-gpu) that
#              already holds the sources, the NDK, Go, the Go module cache and the FetchContent
#              dependencies. It is only read, and nothing is downloaded.
# The reference workspace is used when it is complete, otherwise the inputs are fetched;
# AMUI_LOSSLESS_INPUTS=fetch|reference forces one. AMUI_NDK_ZIP names an android-ndk-r23b-linux.zip
# you already have (it is checked like a downloaded one), AMUI_GO a go binary to use.
# x86_64 hosts only (the service is an x86_64 Android program run natively). No sudo, no system packages.
set -euo pipefail
umask 077
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
eval "$(python3 -I -- "$here/wrapperctl.py" paths --sh)"

# ---- pinned inputs (fetch mode) ----
wrapper_url=https://github.com/WorldObservationLog/wrapper.git
wrapper_pin=386ec82e1fc9c9761aa68060a2be6f19d39106fc
cjson_url=https://github.com/DaveGamble/cJSON.git
cjson_pin=c859b25da02955fef659d658b8f324b5cde87be3        # v1.7.19, what the wrapper's CMakeLists asks for
dobby_url=https://github.com/BepInEx/Dobby.git
dobby_pin=9d271803881f467aabfc24593c7dcaeb710fe861        # the wrapper asks for "master": pinned to the reviewed build's commit
downloader_url=https://github.com/zhaarey/apple-music-downloader.git
downloader_pin=452ba0b672d49afffca3f57b0c2c4670a316e2ca   # 0.1.4
ndk_url=https://dl.google.com/android/repository/android-ndk-r23b-linux.zip
ndk_sha256=c6e97f9c8cfe5b7be0a9e6c15af8e7a179475b7ded23e2d1c1fa0945d6fb4382
ndk_revision=23.1.7779620
go_release=1.27.3
go_url=https://go.dev/dl/go$go_release.linux-amd64.tar.gz
go_sha256=65efbc80fdc1fa2cd2631cad5b481777a68e8725f34e89f50fc6ac714e379b40
ndk_clang=toolchains/llvm/prebuilt/linux-x86_64/bin/x86_64-linux-android22-clang++

build=$AMUI_LOSSLESS_BUILD
log_dir="$build/logs"
mkdir -p -- "$build" "$build/bin" "$log_dir" "$build/tmp"
chmod 700 -- "$build"
log="$log_dir/build-$(date -u +%Y%m%dT%H%M%SZ).log"
trap 'printf "Build stopped. Log: %s\n" "$log" >&2' ERR
exec 3>>"$log"
say() { printf '%s\n' "$*"; printf '%s\n' "$*" >&3; }
stop() { say "$*"; printf 'Build stopped. Log: %s\n' "$log" >&2; exit 1; }

[[ "$(uname -m)" == x86_64 ]] || { printf 'The lossless route needs an x86_64 host (this one is %s).\n' "$(uname -m)" >&2; exit 2; }
for tool in cmake ninja cc c++ python3 sha256sum patch tar git; do
    command -v "$tool" >/dev/null || { printf 'Missing tool: %s\n' "$tool" >&2; exit 2; }
done

# ---- where the inputs come from ----
ref=${AMUI_WORKSPACE:-$HOME/Documents/apple-music-gpu}
stage="$ref/builds/music-downloader"
reference_complete() {
    local path
    for path in "$stage/wrapper-tls-src/CMakeLists.txt" "$stage/wrapper-tls-src/rootfs/system/bin/linker64" \
                "$stage/downloader-src/go.mod" "$stage/toolchains/android-ndk-r23b/$ndk_clang" \
                "$stage/toolchains/go1.27.3/go/bin/go" "$stage/wrapper-build/_deps/cjson-src/cJSON.h" \
                "$stage/wrapper-build/_deps/dobby-src/include/dobby.h" "$stage/go-modules"; do
        [[ -e "$path" ]] || { missing_reference=$path; return 1; }
    done
}
missing_reference=""
inputs=${AMUI_LOSSLESS_INPUTS:-auto}
case "$inputs" in
    auto) if reference_complete; then inputs=reference; else inputs=fetch; fi ;;
    reference) reference_complete || { printf 'Reference input missing: %s\n' "$missing_reference" >&2; exit 2; } ;;
    fetch) ;;
    *) printf 'AMUI_LOSSLESS_INPUTS must be fetch or reference, not %s\n' "$inputs" >&2; exit 2 ;;
esac
say "inputs: $inputs"

sha256_is() { [[ "$(sha256sum -- "$1" | cut -d' ' -f1)" == "$2" ]]; }
download() {  # download <url> <file>: HTTPS only, resumes a partial file
    local progress=-sS
    if [[ -t 2 ]]; then progress=--progress-bar; fi
    mkdir -p -- "$(dirname -- "$2")"
    curl --proto '=https' --tlsv1.2 -fL --retry 3 -C - $progress -o "$2.part" "$1" || { rm -f -- "$2.part"; return 1; }
    mv -- "$2.part" "$2"
}
fetch_commit() {  # fetch_commit <url> <commit> <dir>: a clean checkout of exactly that commit
    local url=$1 commit=$2 dir=$3
    if [[ -d "$dir/.git" && "$(git -C "$dir" rev-parse HEAD 2>/dev/null)" == "$commit" \
          && -z "$(git -C "$dir" status --porcelain 2>/dev/null)" ]]; then
        return 0
    fi
    rm -rf -- "$dir" "$dir.fetch"
    mkdir -p -- "$dir.fetch"
    git -C "$dir.fetch" init --quiet >&3 2>&1
    GIT_TERMINAL_PROMPT=0 git -C "$dir.fetch" fetch --quiet --depth 1 "$url" "$commit" >&3 2>&1 \
        || stop "could not fetch $commit from $url"
    git -C "$dir.fetch" -c advice.detachedHead=false checkout --quiet --detach FETCH_HEAD >&3 2>&1
    [[ "$(git -C "$dir.fetch" rev-parse HEAD)" == "$commit" ]] || stop "$url did not deliver commit $commit"
    mv -- "$dir.fetch" "$dir"
}
version_at_least() { [[ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -1)" == "$2" ]]; }  # <have> <need>

if [[ "$inputs" == reference ]]; then
    wrapper_ref="$stage/wrapper-tls-src"
    downloader_ref="$stage/downloader-src"
    ndk="$stage/toolchains/android-ndk-r23b"
    go_bin="$stage/toolchains/go1.27.3/go/bin/go"
    cjson_src="$build/_deps/cjson-src"
    dobby_src="$build/_deps/dobby-src"
    mkdir -p -- "$build/_deps"
    for dep in cjson-src dobby-src; do  # FetchContent inputs from the reference build, no download
        if [[ ! -d "$build/_deps/$dep" ]]; then
            tar -C "$stage/wrapper-build/_deps" --exclude=.git -cf - "$dep" | tar -C "$build/_deps" -xf -
            chmod -R u+w -- "$build/_deps/$dep"
        fi
    done
else
    for tool in curl unzip; do
        command -v "$tool" >/dev/null || { printf 'Missing tool: %s\n' "$tool" >&2; exit 2; }
    done
    upstream="$build/upstream"
    mkdir -p -- "$upstream" "$build/toolchains" "$build/downloads"
    wrapper_ref="$upstream/wrapper"
    downloader_ref="$upstream/downloader"
    cjson_src="$upstream/cjson"
    dobby_src="$upstream/dobby"
    say "fetching the pinned sources (wrapper, cJSON, Dobby, downloader)..."
    fetch_commit "$wrapper_url" "$wrapper_pin" "$wrapper_ref"
    fetch_commit "$cjson_url" "$cjson_pin" "$cjson_src"
    fetch_commit "$dobby_url" "$dobby_pin" "$dobby_src"
    fetch_commit "$downloader_url" "$downloader_pin" "$downloader_ref"

    ndk="$build/toolchains/android-ndk-r23b"
    if [[ ! -x "$ndk/$ndk_clang" ]] || ! grep -q "^Pkg.Revision = $ndk_revision\$" "$ndk/source.properties" 2>/dev/null; then
        ndk_zip=${AMUI_NDK_ZIP:-$build/downloads/android-ndk-r23b-linux.zip}
        if [[ ! -f "$ndk_zip" ]]; then
            [[ -z "${AMUI_NDK_ZIP:-}" ]] || stop "AMUI_NDK_ZIP names no file: $ndk_zip"
            say "downloading the Android NDK r23b (725 MB)..."
            download "$ndk_url" "$ndk_zip" || stop "the NDK download failed: run the build again to resume"
        fi
        if ! sha256_is "$ndk_zip" "$ndk_sha256"; then
            [[ -n "${AMUI_NDK_ZIP:-}" ]] || rm -f -- "$ndk_zip"
            stop "the NDK archive does not have the pinned SHA-256 ($ndk_sha256)"
        fi
        say "unpacking the NDK (2.3 GB)..."
        rm -rf -- "$ndk" "$build/toolchains/ndk.unpack"
        unzip -q "$ndk_zip" -d "$build/toolchains/ndk.unpack" >&3 2>&1
        mv -- "$build/toolchains/ndk.unpack/android-ndk-r23b" "$ndk"
        rmdir -- "$build/toolchains/ndk.unpack"
        [[ -n "${AMUI_NDK_ZIP:-}" ]] || rm -f -- "$ndk_zip"
        [[ -x "$ndk/$ndk_clang" ]] || stop "the unpacked NDK has no $ndk_clang"
    fi

    go_needed=$(awk '$1 == "go" { print $2; exit }' "$downloader_ref/go.mod")
    go_bin=""
    for candidate in "${AMUI_GO:-}" "$build/toolchains/go$go_release/go/bin/go" "$(command -v go || true)"; do
        [[ -n "$candidate" && -x "$candidate" ]] || continue
        have=$(GOTOOLCHAIN=local XDG_CONFIG_HOME="$build/go-config" "$candidate" env GOVERSION 2>/dev/null | sed -E 's/^go//' || true)
        if [[ -n "$have" ]] && version_at_least "$have" "$go_needed"; then go_bin=$candidate; break; fi
    done
    if [[ -z "$go_bin" ]]; then
        go_tar="$build/downloads/go$go_release.linux-amd64.tar.gz"
        say "no Go $go_needed or later here: downloading Go $go_release (70 MB)..."
        [[ -f "$go_tar" ]] || download "$go_url" "$go_tar" || stop "the Go download failed"
        sha256_is "$go_tar" "$go_sha256" || { rm -f -- "$go_tar"; stop "the Go archive does not have the pinned SHA-256 ($go_sha256)"; }
        rm -rf -- "$build/toolchains/go$go_release" "$build/toolchains/go.unpack"
        mkdir -p -- "$build/toolchains/go.unpack"
        tar -C "$build/toolchains/go.unpack" -xzf "$go_tar"
        mv -- "$build/toolchains/go.unpack" "$build/toolchains/go$go_release"
        rm -f -- "$go_tar"
        go_bin="$build/toolchains/go$go_release/go/bin/go"
        [[ -x "$go_bin" ]] || stop "the unpacked Go has no go/bin/go"
    fi
    say "go: $go_bin ($(GOTOOLCHAIN=local XDG_CONFIG_HOME="$build/go-config" "$go_bin" env GOVERSION 2>/dev/null || echo unknown))"
fi

# ---- 1. wrapper source: a copy without .git, then the patches in order ----
# The reference worktree already carries the TLS repair (patches/0001); a fetched checkout is pristine.
wrapper_commit=$(git -C "$wrapper_ref" rev-parse HEAD)
wrapper_branch=$(git -C "$wrapper_ref" rev-parse --abbrev-ref HEAD)
wrapper_dirty=$(git -C "$wrapper_ref" status --porcelain | tr '\n' ' ')
say "wrapper source: $wrapper_ref ($wrapper_branch at $wrapper_commit; local changes: ${wrapper_dirty:-none})"
if [[ "$inputs" == reference ]]; then
    first_patch=2
    expected_dirty=" M lite/apple_api.cpp  M lite/bridge.cpp  M lite/lite.h  M lite/lite_main.cpp  M lite/tokens.cpp "
    if [[ "$wrapper_dirty" != "$expected_dirty" ]]; then
        say "warning: the TLS worktree's modified-file list differs from the reviewed 2026-11-05 repair; inspect it."
    fi
else
    first_patch=1
fi
src="$build/wrapper-src"
rm -rf -- "$src"
mkdir -p -- "$src"
tar -C "$wrapper_ref" --exclude=.git -cf - . | tar -C "$src" -xf -
chmod -R u+w -- "$src"
for patch_file in "$here"/patches/[0-9][0-9][0-9][0-9]-*.patch; do
    patch_name=$(basename "$patch_file")
    (( 10#${patch_name%%-*} >= first_patch )) || continue
    (cd -- "$src" && patch -p1 --forward --silent <"$patch_file") >&3 2>&1 || { say "failed to apply $patch_name"; exit 1; }
    say "applied patches/$patch_name"
done
# The TLS repair (patches/0001) must really be there, whichever way it came.
grep -q 'workspace-ca-bundle.pem' "$src/lite/apple_api.cpp" || { say "TLS repair missing from the source copy"; exit 1; }
grep -q 'CURLINFO_RESPONSE_CODE = 0x200002;' "$src/lite/lite.h" || { say "TLS repair constant fix missing"; exit 1; }
! grep -q 'CURLOPT_SSL_VERIFYPEER, 0' "$src/lite/apple_api.cpp" || { say "a TLS verification bypass is still present"; exit 1; }
grep -q 'LITE_DNS_SERVERS' "$src/lite/bridge.cpp" || { say "DNS patch not applied"; exit 1; }
grep -q 'enforce_tls_policy' "$src/lite/bridge.cpp" || { say "native curl TLS policy not applied"; exit 1; }
! grep -q 'g_ssl_verify_disabled &&' "$src/lite/bridge.cpp" || { say "the debug TLS-disable hook is still present"; exit 1; }
grep -q 'requested again, aborting login' "$src/lite/auth.cpp" || { say "the sign-in guard (one answer per credential request) is not applied"; exit 1; }

# ---- 2 + 3. compile lite with the NDK (Release); cJSON and Dobby come from the directories chosen above ----
say "configuring wrapper-lite (NDK r23b, Release, FetchContent fully disconnected)..."
export TMPDIR="$build/tmp" XDG_CACHE_HOME="$build/cache"
# lite links the Android libraries of the rootfs. Upstream finds them with find_library(... PATHS
# rootfs), which looks through this system's own library folders first: where the development files
# of a library with the same name are installed (libcurl is the usual one), lite gets linked against
# that one, asks for "libcurl.so.4" inside the rootfs and cannot be loaded. So every library is named
# here (which also replaces a wrong answer kept in an earlier build's CMake cache), and the result is
# checked below.
syslib="$src/rootfs/system/lib64"
lib_args=()
for pair in ANDROIDAPPMUSIC_LIB:androidappmusic STORESERVICESCORE_LIB:storeservicescore \
            MEDIAPLATFORM_LIB:mediaplatform CXX_SHARED_LIB:c++_shared CURL_SHARED_LIB:curl LOG_SHARED_LIB:log; do
    [[ -f "$syslib/lib${pair#*:}.so" ]] || { say "the wrapper source has no rootfs/system/lib64/lib${pair#*:}.so"; exit 1; }
    lib_args+=("-D${pair%%:*}:FILEPATH=$syslib/lib${pair#*:}.so")
done
cmake -S "$src" -B "$build/wrapper-build" -G Ninja \
    -DCMAKE_BUILD_TYPE=Release -DANDROID_NDK_PATH="$ndk" \
    -DBUILD_HOST_LAUNCHERS=OFF -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
    -DFETCHCONTENT_FULLY_DISCONNECTED=ON \
    -DFETCHCONTENT_SOURCE_DIR_CJSON="$cjson_src" \
    -DFETCHCONTENT_SOURCE_DIR_DOBBY="$dobby_src" "${lib_args[@]}" >&3 2>&1
say "compiling wrapper-lite..."
cmake --build "$build/wrapper-build" --parallel "$(nproc)" >&3 2>&1
lite="$src/rootfs/system/bin/lite"
[[ -x "$lite" ]] || { say "lite was not produced"; exit 1; }
# Whatever lite asks the Android loader for must be a file of the rootfs (libdl.so is the loader itself).
needed=$("$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-readelf" -d "$lite" 2>&3 \
         | sed -n 's/.*(NEEDED).*\[\(.*\)\].*/\1/p')
[[ -n "$needed" ]] || { say "could not read which libraries lite needs"; exit 1; }
for lib in $needed; do
    [[ "$lib" == libdl.so || -f "$syslib/$lib" ]] || { say "lite needs $lib, which is not in the rootfs: a library of this system was linked instead of the rootfs's"; exit 1; }
done
say "lite: $(sha256sum "$lite" | cut -d' ' -f1) (needs: $(echo $needed))"

# ---- 4. rootless host launcher ----
cc -O2 -Wall -o "$build/bin/wrapper-lite-rootless.tmp" "$src/wrapper-lite-rootless.c" >&3 2>&1
mv -- "$build/bin/wrapper-lite-rootless.tmp" "$build/bin/wrapper-lite-rootless"
chmod 700 -- "$build/bin/wrapper-lite-rootless"
say "launcher: $(sha256sum "$build/bin/wrapper-lite-rootless" | cut -d' ' -f1)"

# ---- 5. apple-music-downloader (Go; offline from the reference module cache, else through the module proxy) ----
downloader_commit=$(git -C "$downloader_ref" rev-parse HEAD)
downloader_version=$(tr -d '\r\n' <"$downloader_ref/VERSION")
say "downloader source: $downloader_ref ($downloader_version at $downloader_commit)"
amdl_src="$build/amdl-src"
rm -rf -- "$amdl_src"
mkdir -p -- "$amdl_src"
tar -C "$downloader_ref" --exclude=.git -cf - . | tar -C "$amdl_src" -xf -
chmod -R u+w -- "$amdl_src"
unset GONOSUMDB GONOSUMCHECK GONOPROXY GOPRIVATE GOINSECURE GOFLAGS
export GOCACHE="$build/go-cache" GOMODCACHE="$build/go-modules" GOPATH="$build/go-path" GOTMPDIR="$build/tmp"
export GOENV=off GOTOOLCHAIN=local GOTELEMETRY=off GOWORK=off CGO_ENABLED=0
# The go command keeps its telemetry counters in the user's configuration folder and GOTELEMETRY in
# the environment does not change that: give it a configuration folder inside var/.
export XDG_CONFIG_HOME="$build/go-config"
if [[ "$inputs" == reference ]]; then
    if [[ ! -d "$build/go-modules" ]]; then
        say "copying the Go module cache from the reference workspace..."
        tar -C "$stage" -cf - go-modules | tar -C "$build" -xf -
        chmod -R u+w -- "$build/go-modules"
    fi
    export GOPROXY=off GOFLAGS=-mod=readonly
else  # modules are downloaded on demand and must match the downloader's go.sum (go.mod is never rewritten)
    export GOPROXY=https://proxy.golang.org,direct GOSUMDB=sum.golang.org GOFLAGS="-mod=readonly -modcacherw"
fi
say "compiling amdl (GOPROXY=$GOPROXY, -mod=readonly)..."
(cd -- "$amdl_src" && "$go_bin" build -trimpath \
    -ldflags="-X amdl/internal/version.Version=$downloader_version -X amdl/internal/version.Commit=$downloader_commit -X amdl/internal/version.BuildDate=$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    -o "$build/bin/amdl.tmp" .) >&3 2>&1
mv -- "$build/bin/amdl.tmp" "$build/bin/amdl"
chmod 700 -- "$build/bin/amdl"
"$build/bin/amdl" --version >&3 2>&1 || { say "amdl --version failed"; exit 1; }
say "amdl: $(sha256sum "$build/bin/amdl" | cut -d' ' -f1)"

# ---- 5b. am-ui's FairPlay primitive: the downloader's rip/defrag/alacfix internals behind a small CLI
# (tools/lossless/go/amui-fairplay-rip/main.go; built inside the module copy so it can import them).
mkdir -p -- "$amdl_src/cmd/amui-fairplay-rip"
cp -- "$here/go/amui-fairplay-rip/main.go" "$amdl_src/cmd/amui-fairplay-rip/main.go"
say "compiling amui-fairplay-rip..."
(cd -- "$amdl_src" && "$go_bin" build -trimpath -o "$build/bin/amui-fairplay-rip.tmp" ./cmd/amui-fairplay-rip) >&3 2>&1
mv -- "$build/bin/amui-fairplay-rip.tmp" "$build/bin/amui-fairplay-rip"
chmod 700 -- "$build/bin/amui-fairplay-rip"
"$build/bin/amui-fairplay-rip" --version >&3 2>&1 || { say "amui-fairplay-rip --version failed"; exit 1; }
say "amui-fairplay-rip: $(sha256sum "$build/bin/amui-fairplay-rip" | cut -d' ' -f1)"

# ---- 6. build manifest ----
source_fingerprint=$(python3 -I -- "$here/wrapperctl.py" fingerprint)  # setup.sh rebuilds when tools/lossless changed since
python3 -I - "$build" "$wrapper_commit" "$wrapper_branch" "$downloader_commit" "$downloader_version" "$ndk" "$go_bin" "$here" "$inputs" "$source_fingerprint" <<'PY'
import hashlib, json, pathlib, sys, time
build, wcommit, wbranch, dcommit, dversion, ndk, gobin, tools, inputs, fingerprint = sys.argv[1:]
build = pathlib.Path(build)
def sha(p): return hashlib.sha256(pathlib.Path(p).read_bytes()).hexdigest()
manifest = {
    "built_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    "inputs": inputs,
    "source_fingerprint": fingerprint,
    "wrapper": {"upstream": "https://github.com/WorldObservationLog/wrapper (lite)", "commit": wcommit,
                "worktree_branch": wbranch,
                "patches": sorted(p.name for p in (pathlib.Path(tools) / "patches").glob("*.patch")),
                "lite_sha256": sha(build / "wrapper-src/rootfs/system/bin/lite"),
                "launcher_sha256": sha(build / "bin/wrapper-lite-rootless"),
                "ndk": ndk, "build_type": "Release"},
    "downloader": {"upstream": "https://github.com/zhaarey/apple-music-downloader", "commit": dcommit,
                   "version": dversion, "sha256": sha(build / "bin/amdl"), "go": gobin},
    "fairplay_rip": {"source": "tools/lossless/go/amui-fairplay-rip/main.go",
                     "sha256": sha(build / "bin/amui-fairplay-rip")},
}
(build / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
PY

# ---- 7. stage the private runtime (refuses while the service runs; never touches rootfs/data) ----
say "staging the runtime under $AMUI_LOSSLESS_RUNTIME ..."
python3 -I -- "$here/wrapperctl.py" stage | tee -a "$log"

# ---- 8. downloader configuration ----
amdl_dir="$AMUI_LOSSLESS_RUNTIME/amdl"
mkdir -p -- "$amdl_dir/out"
chmod 700 -- "$amdl_dir" "$amdl_dir/out"
python3 -I - "$here/amdl-config.yaml.in" "$amdl_dir/config.yaml.generated" "$AMUI_LOSSLESS_PORT" "$amdl_dir/out" <<'PY'
import pathlib, sys
template, out, port, outdir = sys.argv[1:]
text = pathlib.Path(template).read_text().replace("__PORT__", port).replace("__OUT__", outdir)
pathlib.Path(out).write_text(text)
PY
if [[ ! -f "$amdl_dir/config.yaml" ]]; then
    cp -- "$amdl_dir/config.yaml.generated" "$amdl_dir/config.yaml"
    say "wrote $amdl_dir/config.yaml"
else
    say "kept existing $amdl_dir/config.yaml (fresh template: config.yaml.generated)"
fi
chmod 600 -- "$amdl_dir"/config.yaml*

say "Build complete. Log: $log"
say "Next: sign in with Connect in the am-ui window (or tools/lossless/login.sh in a terminal); am-ui starts the service when it needs it."
