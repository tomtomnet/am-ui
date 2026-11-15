#!/usr/bin/env bash
# Render one input with named probe variants: run-variants.sh INPUT TAG 'name=--opt value ...' ...
# Output: am-ui/var/tmp/cavern-lab/TAG/NAME/{atmos-916.f32le,render-summary.json} and TAG/NAME.log.
set -euo pipefail
umask 077
lab=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
source "$lab/build-env.sh"
input=$1; tag=$2; shift 2
root="$lab/../../var/tmp/cavern-lab/$tag"
mkdir -p "$root"
for spec in "$@"; do
    name=${spec%%=*}; options=${spec#*=}; [ "$name" = "$spec" ] && options=""
    out="$root/$name"; rm -rf "$out"
    # shellcheck disable=SC2086  # the options are meant to be word-split
    if "$dotnet" "$lab/probe/bin/Release/net8.0/CavernProbe.dll" "$input" "$out" $options > "$root/$name.log" 2>&1; then
        printf '%s: %s\n' "$name" "$(tail -1 "$root/$name.log")"
    else
        printf 'FAILED %s\n' "$name"; tail -3 "$root/$name.log"
    fi
done
