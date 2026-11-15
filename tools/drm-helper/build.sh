#!/usr/bin/env bash
# Build the DRM helper the engine talks to over stdin/stdout (config.DRM_HELPER):
#   workspace/builds/music-downloader/amdl-linux-target/release/amdl-linux-probe
#
# The helper is a small harness (workspace/builds/music-downloader/amdl-linux, in this repository)
# over two Rust modules of Kyant0/AMDL (GPL-3.0). Those are not stored here: this script fetches
# the pinned commit next to the harness, checks the files it compiles in against the checksums
# below, and applies patches/ to a copy of the decrypt module (amdl-patched/decrypt.rs) before
# building. The fetched source itself stays as fetched.
#
#   tools/drm-helper/build.sh           fetch when needed, then build
#   tools/drm-helper/build.sh --check   say whether the helper is built from the current sources;
#                                       change nothing. Exit 0 current, 1 not built, 2 out of date
#
# Needs git, patch, a C linker (cc) and cargo 1.85 or later; setup.sh provides cargo when the system has
# none. Network: github.com (the pinned commit) and crates.io (the crates in Cargo.lock, verified by
# cargo against the checksums recorded there). Writes only inside this repository's workspace/ and
# var/ folders; a different AMUI_WORKSPACE is never written.
set -euo pipefail
umask 077
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
amui=$(cd -- "$here/../.." && pwd -P)
amui_var=${AMUI_VAR:-$amui/var}
stage="$amui/workspace/builds/music-downloader"
harness="$stage/amdl-linux"
source_dir="$stage/amdl-android-src"
patched_dir="$stage/amdl-patched"
target_dir="$stage/amdl-linux-target"
helper="$target_dir/release/amdl-linux-probe"
stamp="$helper.inputs"  # fingerprint of the sources the helper was built from

upstream_url=https://github.com/Kyant0/AMDL.git
upstream_commit=01b373d68ed0d4006585ff43e601cd226a05cb34
pinned_files="
077b45de5e243334e864edda3b04d49efeb4b3a375f1399f85b524ebddeb9feb  rust/src/drm.rs
0d5192cdbc8f8d748cc4961019fce3ce434eff835785a5b84521cd15bc468c5d  rust/src/decrypt.rs
2798a7ba7c9a25eca8ddfb741d81c54a6afc43aaaa5521956651f5b39571f881  rust/src/test.wvd
"
minimum_cargo="1 85"  # the harness and the upstream modules use the 2024 edition

say() { printf 'drm-helper: %s\n' "$*"; }
fail() { printf 'drm-helper: %s\n' "$*" >&2; exit 1; }

shopt -s nullglob
patches=("$here"/patches/[0-9][0-9][0-9][0-9]-*.patch)  # applied in this order
shopt -u nullglob

# What a build is made from, as far as this repository decides it: the pin, this script, the patches
# and the harness. A helper built from anything else is out of date.
fingerprint() {
    {
        printf '%s\n%s\n' "$upstream_commit" "$pinned_files"
        cd -- "$amui" && sha256sum -- tools/drm-helper/build.sh "${patches[@]#"$amui/"}" \
            workspace/builds/music-downloader/amdl-linux/Cargo.toml \
            workspace/builds/music-downloader/amdl-linux/Cargo.lock \
            workspace/builds/music-downloader/amdl-linux/src/main.rs
    } | sha256sum | cut -d' ' -f1
}

if [ "${1:-}" = --check ]; then
    if [ ! -x "$helper" ]; then say "not built: $helper"; exit 1; fi
    if [ "$(cat -- "$stamp" 2>/dev/null)" != "$(fingerprint)" ]; then
        say "out of date (built from earlier sources): $helper"; exit 2
    fi
    say "built: $helper"; exit 0
fi

[ -f "$harness/Cargo.toml" ] && [ -f "$harness/Cargo.lock" ] && [ -f "$harness/src/main.rs" ] \
    || fail "the harness is missing from $harness (incomplete checkout?)"
[ "${#patches[@]}" -gt 0 ] || fail "the patches are missing from $here/patches (incomplete checkout?)"
for tool in git cc sha256sum patch; do
    command -v "$tool" >/dev/null || fail "missing program: $tool"
done

# cargo: AMUI_CARGO, the toolchain setup.sh installed (var/toolchains/cargo), the PATH.
cargo=""
for candidate in "${AMUI_CARGO:-}" "$amui_var/toolchains/cargo/bin/cargo" "$(command -v cargo || true)"; do
    if [ -n "$candidate" ] && [ -x "$candidate" ]; then cargo=$candidate; break; fi
done
[ -n "$cargo" ] || fail "no cargo found. Run ./setup.sh, or install Rust 1.85 or later (https://rustup.rs)."
if [ "$cargo" = "$amui_var/toolchains/cargo/bin/cargo" ]; then
    export CARGO_HOME="$amui_var/toolchains/cargo" RUSTUP_HOME="$amui_var/toolchains/rustup"
    export PATH="$CARGO_HOME/bin:$PATH"
else
    export CARGO_HOME="${CARGO_HOME:-$amui_var/build/cargo-home}"  # crate downloads stay inside var/
fi
version=$("$cargo" --version | sed -E 's/^cargo ([0-9]+)\.([0-9]+).*/\1 \2/')
read -r major minor <<<"$version"
read -r want_major want_minor <<<"$minimum_cargo"
if ! [[ "$major" =~ ^[0-9]+$ && "$minor" =~ ^[0-9]+$ ]]; then fail "cannot read the version of $cargo"; fi
if [ "$major" -lt "$want_major" ] || { [ "$major" -eq "$want_major" ] && [ "$minor" -lt "$want_minor" ]; }; then
    fail "$cargo is version $major.$minor; 1.85 or later is needed. Run ./setup.sh (it installs a private toolchain)."
fi

# ---- the pinned upstream source ----
verify_source() {
    [ "$(git -C "$source_dir" rev-parse HEAD 2>/dev/null)" = "$upstream_commit" ] || return 1
    (cd -- "$source_dir" && printf '%s' "$pinned_files" | sed '/^$/d' | sha256sum --check --quiet --strict -) >/dev/null 2>&1
}
if verify_source; then
    say "upstream source present ($upstream_commit)"
else
    say "fetching Kyant0/AMDL at $upstream_commit ..."
    rm -rf -- "$source_dir.fetch"
    mkdir -p -- "$source_dir.fetch"
    git -C "$source_dir.fetch" init --quiet
    GIT_TERMINAL_PROMPT=0 git -C "$source_dir.fetch" fetch --quiet --depth 1 "$upstream_url" "$upstream_commit" \
        || fail "could not fetch $upstream_url. Without it, copy a built amdl-linux-probe from another machine to $helper"
    git -C "$source_dir.fetch" -c advice.detachedHead=false checkout --quiet FETCH_HEAD
    rm -rf -- "$source_dir"
    mv -- "$source_dir.fetch" "$source_dir"
    verify_source || fail "the fetched source does not match the pinned commit and checksums; not building"
fi

# ---- the decrypt module the harness compiles: the pinned file with patches/ applied ----
rm -rf -- "$patched_dir.new"
mkdir -p -- "$patched_dir.new/rust/src" "$patched_dir"
cp -- "$source_dir/rust/src/decrypt.rs" "$patched_dir.new/rust/src/decrypt.rs"
for patch_file in "${patches[@]}"; do
    (cd -- "$patched_dir.new" && patch -p1 --forward --silent --no-backup-if-mismatch <"$patch_file") \
        || fail "could not apply patches/$(basename -- "$patch_file")"
    say "applied patches/$(basename -- "$patch_file")"
done
# an unchanged file keeps its time, so cargo has nothing to compile again
cmp -s -- "$patched_dir.new/rust/src/decrypt.rs" "$patched_dir/decrypt.rs" \
    || mv -f -- "$patched_dir.new/rust/src/decrypt.rs" "$patched_dir/decrypt.rs"
rm -rf -- "$patched_dir.new"

# ---- build ----
rm -f -- "$stamp"
mkdir -p -- "$amui_var/build" "$target_dir"
log="$amui_var/build/drm-helper-build.log"
say "building with $("$cargo" --version) ..."
if ! "$cargo" build --release --locked --manifest-path "$harness/Cargo.toml" --target-dir "$target_dir" >"$log" 2>&1; then
    tail -25 -- "$log" >&2
    fail "build failed, see $log"
fi
[ -x "$helper" ] || fail "the build produced no helper at $helper"
chmod 755 -- "$helper"
fingerprint >"$stamp"
say "built $helper ($(sha256sum "$helper" | cut -c1-16)…)"
