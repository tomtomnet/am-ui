#!/usr/bin/env bash
# Builds pipewire_chain (see pipewire_chain.c) in a folder of its own and prints the SHA-256 of
# what PipeWire's filter code gives for the cases of dsp::tests::matches_pipewire_s_own_code.
#
#   crates/engine/tests/reference/pipewire_chain.sh [SOURCES]
#
# SOURCES is a folder with biquad.c, biquad.h and audio-dsp-c.c of PipeWire 1.6.11; without it
# they are fetched from PipeWire's repository into a temporary folder.
set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
if [ $# -ge 1 ]; then
    cp "$1"/biquad.c "$1"/biquad.h "$1"/audio-dsp-c.c "$work"/
else
    base=https://gitlab.freedesktop.org/pipewire/pipewire/-/raw/1.6.11/spa/plugins
    curl -sSf -o "$work/biquad.c" "$base/audioconvert/biquad.c"
    curl -sSf -o "$work/biquad.h" "$base/filter-graph/biquad.h"
    curl -sSf -o "$work/audio-dsp-c.c" "$base/filter-graph/audio-dsp-c.c"
fi
# the plain C loops: from the first function to the end of the delay, before the FFT helpers
first=$(grep -n '^void dsp_clear_c' "$work/audio-dsp-c.c" | cut -d: -f1)
last=$(grep -n '^#ifdef HAVE_FFTW' "$work/audio-dsp-c.c" | sed -n 2p | cut -d: -f1)
sed -n "${first},$((last - 1))p" "$work/audio-dsp-c.c" > "$work/audio-dsp-part.inc"
# no fused multiply-add and no reordering: the arithmetic as it is written
cc -std=gnu11 -O2 -ffp-contract=off -I"$work" -o "$work/pipewire_chain" "$here/pipewire_chain.c" -lm

peqdb="1 0.70710678 1 1 1 1 1 1 2.26464 125 0.5411961 1.306563 0.0033375519"
run() { printf '%-22s ' "$1"; shift; "$work/pipewire_chain" 48000 "$@" | sha256sum | cut -d' ' -f1; }
run peqdb $peqdb 0.35481339 1
run peqdb-with-equalizer $peqdb 0.35481339 0.30199517 \
    lsc 105 0.7 5.5 lsc 60 0.9 -1.25 pk 20 0.5 -3.9 pk 105 0.8 2.1 pk 210 1.4 -1.2 pk 480 1.1 0.9 \
    pk 900 2 -2.4 pk 1500 1.8 1.6 pk 2200 2.5 4 pk 3100 3 -3.3 pk 4300 4 2.2 pk 6500 4 -6 \
    pk 8200 5 1.1 pk 11000 3.5 -1.7 hsc 10000 0.7 -2.5
run apple 1 0.70710678 0.70710678 0.70710678 0.70710678 0.70710678 0.70710678 0.70710678 0 1000000 0.5411961 1.306563 0 1 1
run itu 1 0.70710678 0.70710678 0.70710678 0.866 0.5 0.5 0.5 0.35355339 1000000 0.5411961 1.306563 0 0.42657952 1
run equal-gains 0.5 0.5 0.5 0.5 0.5 0.5 0.5 0.5 0.5 80 0.5411961 1.306563 0.01 0.75 1.25 pk 1000 1 3
