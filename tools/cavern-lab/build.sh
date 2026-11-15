#!/usr/bin/env bash
# Build the cavern-lab probe (Cavern copy with the lab switches + instrumented CavernProbe).
# Output: tools/cavern-lab/probe/bin/Release/net8.0/CavernProbe.dll. All writable state goes under am-ui/var/build.
set -euo pipefail
umask 077
lab=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
source "$lab/build-env.sh"
log="$amui_var/build/cavern-lab-build.log"
export XDG_DATA_HOME="$amui_var/build/dotnet-data"  # NuGet's own bookkeeping stays inside var/ too
for attempt in 1 2; do  # a NuGet named-mutex error can hit the first restore
    # no build servers left running afterwards (MSBuild worker nodes, the shared compiler)
    if "$dotnet" build "$lab/probe/CavernProbe.csproj" -c Release --nologo -nodeReuse:false -p:UseSharedCompilation=false >"$log" 2>&1; then
        printf 'cavern-lab probe build passed (%s).\n' "$lab/probe/bin/Release/net8.0/CavernProbe.dll"
        exit 0
    fi
    sleep 2
done
tail -20 "$log"; printf 'cavern-lab probe build FAILED, see %s\n' "$log" >&2; exit 1
