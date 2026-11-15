#!/usr/bin/env bash
# The Atmos renderer am-ui runs (config.RENDER_SCRIPT): the cavern-lab probe with the
# recommended decoder fixes. Usage: render-music-916-lab.sh input.m4a output-directory [extra probe options]
# Output: <dir>/atmos-916.f32le (16-ch f32le, FL FR FC LFE SL SR RL RR FLW FRW TFL TFR TSL TSR TRL TRR) + render-summary.json.
set -euo pipefail
umask 077
lab=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
source "$lab/build-env.sh"
# CAVERN_LAB_OPTIONS overrides the recommended switches (see research/cavern-artifact.md).
options=${CAVERN_LAB_OPTIONS-"--joc-timeslot-fix --joc-interp-fix --surround-phase -1 --lfe-gain 1.41421356 --lfe-delay 577"}
# shellcheck disable=SC2086
exec "$dotnet" "$lab/probe/bin/Release/net8.0/CavernProbe.dll" "$@" $options
