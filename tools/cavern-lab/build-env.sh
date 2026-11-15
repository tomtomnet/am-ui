#!/usr/bin/env bash
# Shared build/run environment for the cavern-lab probes. Source this file.
# Everything writable lives under am-ui/var/build (AMUI_VAR moves it).
lab=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
amui=$(cd -- "$lab/../.." && pwd -P)
amui_var=${AMUI_VAR:-$amui/var}
# dotnet, first one that exists: AMUI_DOTNET, the SDK setup.sh installed (var/toolchains/dotnet), the
# SDK an older workspace recorded (builds/music-downloader/dotnet-path.txt), the PATH.
dotnet=""
_dotnet_candidates=("${AMUI_DOTNET:-}" "$amui_var/toolchains/dotnet/dotnet")
for _ws in "${AMUI_WORKSPACE:-}" "$HOME/Documents/apple-music-gpu"; do
    if [ -n "$_ws" ] && [ -r "$_ws/builds/music-downloader/dotnet-path.txt" ]; then
        _dotnet_candidates+=("$(cat "$_ws/builds/music-downloader/dotnet-path.txt")")
    fi
done
_dotnet_candidates+=("$(command -v dotnet || true)")
for _candidate in "${_dotnet_candidates[@]}"; do
    if [ -n "$_candidate" ] && [ -x "$_candidate" ]; then dotnet=$_candidate; break; fi
done
unset _dotnet_candidates _candidate _ws
if [ -z "$dotnet" ]; then
    echo "cavern-lab: no dotnet found. Run ./setup.sh (installs a private SDK under var/toolchains), or install dotnet-sdk-8.0 or later." >&2
    return 1 2>/dev/null || exit 1  # this file is sourced by build.sh
fi
export DOTNET_ROOT="$(dirname -- "$(readlink -f -- "$dotnet")")"
export DOTNET_CLI_HOME="$amui_var/build/dotnet-home" NUGET_PACKAGES="$amui_var/build/nuget-packages"
export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_SKIP_FIRST_TIME_EXPERIENCE=1 DOTNET_NOLOGO=1 DOTNET_GENERATE_ASPNET_CERTIFICATE=0
# The probe targets net8.0; a machine that only has a newer runtime runs it on that one.
export DOTNET_ROLL_FORWARD="${DOTNET_ROLL_FORWARD:-Major}"
# .NET refuses to start without ICU unless told to do without; the probe parses and prints numbers
# with the invariant culture only.
if [ -z "${DOTNET_SYSTEM_GLOBALIZATION_INVARIANT:-}" ] && \
   ! { ldconfig -p 2>/dev/null || /sbin/ldconfig -p 2>/dev/null; } | grep -q 'libicuuc\.so'; then
    export DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1
fi
export TMPDIR="$amui_var/build/scratch" XDG_CONFIG_HOME="$amui_var/build/cavern-config"
mkdir -p "$DOTNET_CLI_HOME" "$NUGET_PACKAGES" "$TMPDIR" "$XDG_CONFIG_HOME"
