#!/usr/bin/env bash
# Set up am-ui on this machine from a fresh clone. Safe to run again: every step looks first.
#
#   ./setup.sh                everything needed to play: Dolby Atmos, lossless (ALAC) and stereo
#   ./setup.sh --no-lossless  leave out the lossless route (saves about 1 GB of downloads and 3 GB on
#                             disk). It also runs Apple's native sign-in, the app's only sign-in:
#                             without it the app can use only a sign-in made earlier on this machine
#   ./setup.sh --check        change nothing: report what is there and what is missing
#
# Steps:
#   1. system programs and libraries: checked, never installed. For missing ones it prints the install
#      command of your distribution and goes on with what it can do (it never runs sudo).
#   2. Rust: cargo 1.88 or later from the PATH, else a private toolchain in var/toolchains
#   3. the application: cargo build --release (target/release/am-ui, and the engine's command line
#      target/release/amui)
#   4. .NET SDK 8 or later from the PATH, else a private one in var/toolchains/dotnet
#   5. the Atmos renderer (tools/cavern-lab/build.sh)
#   6. the DRM helper (tools/drm-helper/build.sh: fetches a pinned upstream commit), built again when
#      its sources in this repository changed
#   7. the lossless route (tools/lossless/build.sh), which is also what runs Apple's native sign-in.
#      x86_64 computers only; skipped elsewhere. Built once, and again only after tools/lossless changed.
#   8. the doctor (./am-ui cli doctor)
#
# Everything downloaded or built stays inside this folder (var/, workspace/, target/). Nothing here
# signs in to Apple Music: do that afterwards with the app's Sign In button.
#
# The first version of am-ui (Qt Quick and Python) needed PySide6 and a PipeWire sink; this one plays
# through the desktop's sound server and needs neither. --venv and --no-sink, options of that time,
# are accepted and do nothing.
set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
cd "$here"
var=${AMUI_VAR:-$here/var}

check_only=0 want_lossless=auto
for argument in "$@"; do
    case "$argument" in
        --check) check_only=1 ;;
        --lossless) want_lossless=1 ;;  # the default on x86_64; kept for older instructions
        --no-lossless) want_lossless=0 ;;
        --venv|--no-sink) ;;            # options of the first version
        -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) printf 'setup: unknown option %s (see ./setup.sh --help)\n' "$argument" >&2; exit 2 ;;
    esac
done

lossless_skipped="--no-lossless"
if [ "$want_lossless" = auto ]; then
    if [ "$(uname -m)" = x86_64 ]; then
        want_lossless=1
    else
        want_lossless=0
        lossless_skipped="this computer is $(uname -m); the route runs on x86_64 only"
    fi
fi

step() { printf '\n== %s\n' "$*"; }
ok() { printf '  ok       %s\n' "$*"; }
note() { printf '  note     %s\n' "$*"; }
did() { printf '  done     %s\n' "$*"; }
bad() { printf '  MISSING  %s\n' "$*"; problems=$((problems + 1)); }
die() { printf '\nsetup: %s\n' "$*" >&2; exit 1; }
problems=0
app="$here/target/release/am-ui"
engine="$here/target/release/amui"

# ----------------------------------------------------------------------------- 1. system programs
step "1. System programs and libraries"
family=unknown
if [ -r /etc/os-release ]; then
    # shellcheck disable=SC1091
    ids=$(. /etc/os-release; printf '%s %s' "${ID:-}" "${ID_LIKE:-}")
    case " $ids " in
        *" fedora "*|*" rhel "*|*" centos "*) family=fedora ;;
        *" debian "*|*" ubuntu "*) family=debian ;;
        *" arch "*) family=arch ;;
        *" suse "*|*" opensuse "*|*" opensuse-tumbleweed "*|*" opensuse-leap "*) family=suse ;;
    esac
fi
# package that provides a program or a library's development files, per distribution family
package_for() {
    case "$family:$1" in
        arch:python3) echo python ;;               *:python3) echo python3 ;;
        *:git) echo git ;;                         *:curl) echo curl ;;
        fedora:ffmpeg) echo ffmpeg-free ;;         *:ffmpeg) echo ffmpeg ;;
        *:cc) echo gcc ;;
        fedora:pkg-config) echo pkgconf-pkg-config ;; arch:pkg-config) echo pkgconf ;;
        *:pkg-config) echo pkg-config ;;
        fedora:gtk4) echo gtk4-devel ;;            debian:gtk4) echo libgtk-4-dev ;;
        arch:gtk4) echo gtk4 ;;                    suse:gtk4) echo gtk4-devel ;;
        fedora:libadwaita) echo libadwaita-devel ;; debian:libadwaita) echo libadwaita-1-dev ;;
        arch:libadwaita) echo libadwaita ;;        suse:libadwaita) echo libadwaita-devel ;;
        fedora:libpulse) echo pulseaudio-libs-devel ;; debian:libpulse) echo libpulse-dev ;;
        arch:libpulse) echo libpulse ;;            suse:libpulse) echo libpulse-devel ;;
        *:cmake) echo cmake ;;                     fedora:ninja) echo ninja-build ;;
        debian:ninja) echo ninja-build ;;          *:ninja) echo ninja ;;
        fedora:c++) echo gcc-c++ ;;                debian:c++) echo g++ ;;
        arch:c++) echo gcc ;;                      suse:c++) echo gcc-c++ ;;
        *:patch) echo patch ;;                     *:unzip) echo unzip ;;
        *) echo "$1" ;;
    esac
}
install_command() {
    case "$family" in
        fedora) echo "sudo dnf install $*" ;;
        debian) echo "sudo apt install $*" ;;
        arch) echo "sudo pacman -S --needed $*" ;;
        suse) echo "sudo zypper install $*" ;;
        *) echo "install with your package manager: $*" ;;
    esac
}
missing_packages=""
need() {  # need <label> <package key> <why> <program...>   (any one of the programs is enough)
    local label=$1 key=$2 why=$3 found=""
    shift 3
    for program in "$@"; do
        if command -v "$program" >/dev/null 2>&1; then found=$(command -v "$program"); break; fi
    done
    if [ -n "$found" ]; then ok "$label ($found)"; return 0; fi
    bad "$label: $why"
    missing_packages="$missing_packages $(package_for "$key")"
    return 1
}
libraries_ok=1 too_old=""
need_library() {  # need_library <label> <package key> <pkg-config module> <minimum version>
    local label=$1 key=$2 module=$3 minimum=$4 have
    if ! command -v pkg-config >/dev/null 2>&1; then
        libraries_ok=0
        missing_packages="$missing_packages $(package_for "$key")"
        return 1
    fi
    if ! have=$(pkg-config --modversion "$module" 2>/dev/null); then
        bad "$label: its development files are missing (the build links the application against it)"
        missing_packages="$missing_packages $(package_for "$key")"
        libraries_ok=0
        return 1
    fi
    if ! pkg-config --atleast-version="$minimum" "$module"; then
        bad "$label $have: the application needs $minimum or later"
        too_old="$too_old $label"
        libraries_ok=0
        return 1
    fi
    ok "$label $have"
}
blocked=0
need git git "fetches the pinned sources" git || blocked=1
need curl curl "downloads the private toolchains" curl || true
need cc cc "links the application and the DRM helper" cc gcc clang || true
need pkg-config pkg-config "finds the libraries below for the build" pkg-config pkgconf || true
need patch patch "applies this repository's patches to the fetched sources" patch || true
need ffmpeg ffmpeg "decodes AAC/ALAC, measures loudness" ffmpeg || true
need ffprobe ffmpeg "comes with ffmpeg" ffprobe || true
need python3 python3 "runs the build scripts of the lossless route" python3 || true
need_library "GTK 4" gtk4 gtk4 4.20 || true
need_library "libadwaita" libadwaita libadwaita-1 1.8 || true
need_library "PulseAudio client library" libpulse libpulse 13.0 || true
if [ "$want_lossless" = 1 ]; then
    need cmake cmake "builds the lossless service" cmake || true
    need ninja ninja "builds the lossless service" ninja || true
    need c++ c++ "builds the lossless service" c++ g++ clang++ || true
    need unzip unzip "unpacks the Android NDK" unzip || true
fi
if [ -n "$missing_packages" ]; then
    # shellcheck disable=SC2046,SC2086
    printf '\n  Install what is missing, then run ./setup.sh again:\n    %s\n' \
        "$(install_command $(printf '%s\n' $missing_packages | sort -u | tr '\n' ' '))"
fi
if [ -n "$too_old" ]; then
    printf '\n  Too old here:%s. The application is built for GNOME 49 or later (GTK 4.20,\n' "$too_old"
    printf '  libadwaita 1.8), as in Fedora 43, Ubuntu 25.10, Arch or openSUSE Tumbleweed.\n'
fi
if [ "$blocked" = 1 ]; then die "git is needed before anything else can be done."; fi

if [ "$check_only" = 1 ]; then
    step "Doctor"
    if [ -x "$engine" ]; then exec "$engine" doctor; fi
    bad "the application is not built yet (step 3 of ./setup.sh): no doctor to ask"
    exit 1
fi

mkdir -p -- "$var"
chmod 700 -- "$var"
mkdir -p -- "$var/toolchains" "$var/build/scratch"
scratch="$var/build/scratch"  # the installers' temporary files stay inside this folder too

# ----------------------------------------------------------------------------- 2. Rust
step "2. Rust (builds the application and the DRM helper)"
private_cargo="$var/toolchains/cargo/bin/cargo"
cargo_version() {  # "cargo --version"; the private toolchain only answers with its own homes set
    if [ "$1" = "$private_cargo" ]; then
        CARGO_HOME="$var/toolchains/cargo" RUSTUP_HOME="$var/toolchains/rustup" "$1" --version
    else
        "$1" --version
    fi
}
cargo_ok() {  # cargo 1.88 or later?
    local version
    version=$(cargo_version "$1" 2>/dev/null | sed -E 's/^cargo ([0-9]+)\.([0-9]+).*/\1 \2/') || return 1
    # shellcheck disable=SC2086
    set -- $version
    [[ "${1:-}" =~ ^[0-9]+$ && "${2:-}" =~ ^[0-9]+$ ]] || return 1
    [ "$1" -gt 1 ] || { [ "$1" -eq 1 ] && [ "$2" -ge 88 ]; }
}
cargo=""
for candidate in "${AMUI_CARGO:-}" "$private_cargo" "$(command -v cargo || true)"; do
    if [ -n "$candidate" ] && [ -x "$candidate" ] && cargo_ok "$candidate"; then cargo=$candidate; break; fi
done
if [ -n "$cargo" ]; then
    ok "$(cargo_version "$cargo") ($cargo)"
elif command -v curl >/dev/null 2>&1; then
    note "no cargo 1.88+ found: installing a private Rust toolchain into var/toolchains (about 350 MB)..."
    if curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o "$var/toolchains/rustup-init.sh" \
        && TMPDIR="$scratch" CARGO_HOME="$var/toolchains/cargo" RUSTUP_HOME="$var/toolchains/rustup" \
           sh "$var/toolchains/rustup-init.sh" -y --no-modify-path --profile minimal --default-toolchain stable \
           >"$var/build/rustup-install.log" 2>&1 \
        && [ -x "$private_cargo" ] && cargo_ok "$private_cargo"; then
        cargo=$private_cargo
        did "$(cargo_version "$cargo") (var/toolchains/cargo)"
    else
        tail -5 -- "$var/build/rustup-install.log" 2>/dev/null | sed 's/^/           /'
        bad "Rust: the download failed (log: var/build/rustup-install.log). Installing cargo 1.88+ from your distribution works too."
    fi
else
    bad "Rust: no cargo 1.88+ found, and curl is missing to download one"
fi
# the private toolchain answers only with its own homes; crate downloads stay inside var/ otherwise
run_cargo() {
    if [ "$cargo" = "$private_cargo" ]; then
        CARGO_HOME="$var/toolchains/cargo" RUSTUP_HOME="$var/toolchains/rustup" "$cargo" "$@"
    else
        CARGO_HOME="${CARGO_HOME:-$var/build/cargo-home}" "$cargo" "$@"
    fi
}

# ----------------------------------------------------------------------------- 3. the application
step "3. The application (target/release/am-ui and target/release/amui)"
if [ -z "$cargo" ]; then
    if [ -x "$app" ]; then note "kept the existing build (no cargo to build it again)"; else bad "application: not built (needs the Rust of step 2)"; fi
elif [ "$libraries_ok" = 0 ]; then
    if [ -x "$app" ]; then note "kept the existing build (the libraries of step 1 are missing or too old)"; else bad "application: not built (needs the libraries of step 1)"; fi
else
    note "building (the first time: about 300 MB of crates and a few minutes)..."
    if TMPDIR="$scratch" run_cargo build --release --locked -p am-ui -p amui >"$var/build/app-build.log" 2>&1 \
        && [ -x "$app" ] && [ -x "$engine" ]; then
        ok "built (target/release/am-ui, target/release/amui)"
    else
        tail -15 -- "$var/build/app-build.log" | sed 's/^/           /'
        bad "application: the build failed (log: var/build/app-build.log)"
    fi
fi

# ----------------------------------------------------------------------------- 4. .NET SDK
step "4. .NET SDK (builds and runs the Atmos renderer)"
sdk_major() {  # highest SDK major version of a dotnet binary, 0 when it has none
    "$1" --list-sdks 2>/dev/null | sed -E 's/^([0-9]+)\..*/\1/' | sort -n | tail -1 | grep -E '^[0-9]+$' || echo 0
}
find_dotnet() { (source "$here/tools/cavern-lab/build-env.sh" >/dev/null 2>&1 && printf '%s' "$dotnet") || true; }
dotnet=$(find_dotnet)
if [ -n "$dotnet" ] && [ "$(sdk_major "$dotnet")" -ge 8 ]; then
    ok "dotnet SDK $("$dotnet" --version 2>/dev/null || echo '?') ($dotnet)"
elif ! command -v curl >/dev/null 2>&1; then
    bad ".NET SDK: none found, and curl is missing to download one"
    dotnet=""
else
    note "no .NET SDK 8+ found: installing one into var/toolchains/dotnet (about 220 MB)..."
    if curl --proto '=https' --tlsv1.2 -fsSL https://dot.net/v1/dotnet-install.sh -o "$var/toolchains/dotnet-install.sh" \
        && TMPDIR="$scratch" bash "$var/toolchains/dotnet-install.sh" --channel 8.0 --install-dir "$var/toolchains/dotnet" --no-path \
               >"$var/build/dotnet-install.log" 2>&1 \
        && [ -x "$var/toolchains/dotnet/dotnet" ]; then
        dotnet="$var/toolchains/dotnet/dotnet"
        did "dotnet SDK $(AMUI_DOTNET="$dotnet" bash -c "source '$here/tools/cavern-lab/build-env.sh' && \"\$dotnet\" --version" 2>/dev/null || echo installed) (var/toolchains/dotnet)"
    else
        tail -5 -- "$var/build/dotnet-install.log" 2>/dev/null | sed 's/^/           /'
        bad ".NET SDK: the download failed (log: var/build/dotnet-install.log). Installing dotnet-sdk-8.0 from your distribution works too."
        dotnet=""
    fi
fi

# ----------------------------------------------------------------------------- 5. renderer
step "5. Atmos renderer (patched Cavern, tools/cavern-lab)"
renderer="$here/tools/cavern-lab/probe/bin/Release/net8.0/CavernProbe.dll"
if [ -z "$dotnet" ]; then
    if [ -f "$renderer" ]; then note "kept the existing build (no SDK to rebuild it)"; else bad "renderer: not built (needs the .NET SDK of step 4)"; fi
elif "$here/tools/cavern-lab/build.sh" >"$var/build/setup-renderer.log" 2>&1; then
    ok "built (tools/cavern-lab/probe/bin/Release/net8.0/CavernProbe.dll)"
else
    tail -12 -- "$var/build/setup-renderer.log" | sed 's/^/           /'
    bad "renderer: the build failed (log: var/build/cavern-lab-build.log)"
fi

# ----------------------------------------------------------------------------- 6. DRM helper
step "6. DRM helper (tools/drm-helper)"
# Always this repository's own build (the engine prefers it to a helper another workspace carries).
helper="$here/workspace/builds/music-downloader/amdl-linux-target/release/amdl-linux-probe"
helper_state=0  # 0 built from the current sources, 1 not built, 2 built from earlier sources
"$here/tools/drm-helper/build.sh" --check >/dev/null 2>&1 || helper_state=$?
if [ "$helper_state" = 0 ]; then
    ok "built from the current sources ($helper)"
elif [ -z "$cargo" ]; then
    bad "DRM helper: not built from the current sources (needs the Rust of step 2)"
else
    if [ "$helper_state" = 2 ]; then note "the helper was built from earlier sources: building it again"; fi
    if AMUI_CARGO="$cargo" "$here/tools/drm-helper/build.sh" 2>&1 | sed 's/^/  /'; then
        [ -x "$helper" ] && ok "built ($helper)" || bad "DRM helper: the build left no binary"
    else
        bad "DRM helper: the build failed (log: var/build/drm-helper-build.log)"
    fi
fi

# ----------------------------------------------------------------------------- 7. lossless route
step "7. Lossless (ALAC) route and Apple's native sign-in (tools/lossless)"
update_waiting=0
# Apple's component runs in a private user namespace, which some systems do not allow ordinary programs.
# The check needs no account and contacts nobody (the component prints its usage text and ends).
lossless_start_check() {
    local reason
    if reason=$(python3 -I -- "$here/tools/lossless/wrapperctl.py" selftest 2>&1 >/dev/null); then
        ok "Apple's component starts on this computer"
    else
        bad "lossless route: ${reason#error: }. Until that is solved the app cannot sign in (it uses Apple's native sign-in)."
    fi
}
if [ "$want_lossless" = 0 ]; then
    note "skipped ($lossless_skipped): without it the app cannot sign in on its own and plays AAC instead of lossless"
elif ! command -v python3 >/dev/null 2>&1; then
    bad "lossless route: python3 (step 1) runs its build scripts"
else
    # absent | stale (built before tools/lossless changed) | unstaged | current, then running | stopped
    lossless_state="absent stopped"
    lossless_state=$(python3 -I -- "$here/tools/lossless/wrapperctl.py" build-state 2>/dev/null) || lossless_state="absent stopped"
    case "$lossless_state" in
        "current "*)
            ok "built (var/build/lossless) and staged (var/wrapper)"
            lossless_start_check ;;
        "absent running")
            bad "lossless route: not built, and something already answers on the service's port (127.0.0.1:${LOSSLESS_PORT:-12351}). Stop it or pick another port (LOSSLESS_PORT), then run ./setup.sh again." ;;
        *" running")
            update_waiting=1
            note "an earlier build is in use and its service is running: left as it is."
            note "To update it: quit am-ui, run tools/lossless/stop.sh, then ./setup.sh again."
            case "$lossless_state" in "stale "*) lossless_start_check ;; esac ;;
        *)
            note "building (the first time: about 1 GB of downloads and 3 GB on disk, several minutes)..."
            if "$here/tools/lossless/build.sh" | sed 's/^/           /'; then
                did "built. The app's Sign In button signs in with Apple's native sign-in and turns lossless on."
                lossless_start_check
            else
                bad "lossless route: the build failed (log under var/build/lossless/logs)."
            fi ;;
    esac
fi

# ----------------------------------------------------------------------------- 8. doctor
step "8. Doctor"
doctor_status=0
if [ -x "$engine" ]; then
    "$engine" doctor || doctor_status=$?
else
    bad "the application is not built (step 3): no doctor to ask"
fi

printf '\n'
if [ "$problems" -gt 0 ] || [ "$doctor_status" -ne 0 ]; then
    printf 'Setup is not complete: see the MISSING lines above, fix them and run ./setup.sh again.\n'
    exit 1
fi
printf 'Setup complete. Start the app with ./am-ui and sign in with its Sign In button (once per machine).\n'
if [ "$update_waiting" = 1 ]; then
    printf 'One update is waiting: the lossless route (see the notes of step 7).\n'
fi
