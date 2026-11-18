#!/usr/bin/env bash
# Remove what ./setup.sh and am-ui made on this computer.
#
#   ./uninstall.sh              everything: the application menu entry and icon, the builds and
#                               private toolchains, and your sign-in, settings, caches and logs
#   ./uninstall.sh --keep-data  keep the sign-in, settings, caches and logs for a later ./setup.sh
#   ./uninstall.sh --yes        do not ask first
#
# It stops the lossless service and refuses to go on while am-ui is running. Only files it knows
# are removed: the source code, your own notes and your system's packages stay. Delete this folder
# afterwards to remove the rest.
set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
cd "$here"

die() { printf 'uninstall: %s\n' "$*" >&2; exit 1; }

keep_data=0 assume_yes=0
for argument in "$@"; do
    case "$argument" in
        --keep-data) keep_data=1 ;;
        --yes|-y) assume_yes=1 ;;
        -h|--help) sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option $argument (see ./uninstall.sh --help)" ;;
    esac
done

# Everything removed lies in this folder, which must really be am-ui's, apart from the menu entry
# and its icon. A folder named by AMUI_VAR is never deleted: that path could be anything.
for file in setup.sh am-ui Cargo.toml tools/lossless/stop.sh; do
    [ -f "$here/$file" ] || die "$here is not an am-ui folder"
done
if [ -n "${AMUI_VAR:-}" ]; then
    die "AMUI_VAR is set: unset it to uninstall this folder, and remove that data folder yourself"
fi
var=$here/var

data_home=${XDG_DATA_HOME:-$HOME/.local/share}
applications=$data_home/applications
entry=$applications/dev.amui.Music.desktop
icon=$data_home/icons/hicolor/scalable/apps/dev.amui.Music.svg

# Programs of this folder that are running, by the file they were started from.
running_from() {
    local link target prefix
    for link in /proc/[0-9]*/exe; do
        target=$(readlink -- "$link" 2>/dev/null) || continue
        for prefix in "$@"; do
            case "$target" in "$prefix"/*) printf '%s\n' "${target##*/}"; break ;; esac
        done
    done | sort -u | tr '\n' ' '
}
busy=$(running_from "$here/target" "$var/toolchains")
if [ -n "$busy" ]; then
    die "still running from this folder: ${busy% }. Quit am-ui, or let a build finish, and try again"
fi

printf 'This removes from this computer:\n'
printf '  - the application menu entry and its icon\n'
printf '  - everything ./setup.sh built, with its private toolchains\n'
if [ "$keep_data" = 1 ]; then
    printf 'Your Apple Music sign-in, settings, caches and logs stay in %s.\n' "$var"
else
    printf '  - your Apple Music sign-in on this computer, your settings, caches and logs (%s)\n' "$var"
fi
printf 'The source code in %s stays.\n' "$here"
if [ "$assume_yes" = 0 ]; then
    [ -t 0 ] || die "nothing was changed: run it in a terminal, or pass --yes"
    read -r -p 'Continue? [y/N] ' answer
    case "$answer" in y|Y|yes|YES|Yes) ;; *) printf 'Nothing was changed.\n'; exit 0 ;; esac
fi

# Folders of the lossless service can be read-only: they are made removable first.
remove() {
    local path
    for path in "$@"; do
        case "$path" in
            "$here"/?*|"$entry"|"$icon") ;;
            *) die "not removing $path: it is outside this folder" ;;
        esac
        if [ -L "$path" ]; then
            rm -f -- "$path"
        elif [ -e "$path" ]; then
            chmod -R u+rwX -- "$path" 2>/dev/null || true
            rm -rf -- "$path"
        fi
    done
}

# Only the service this folder started is stopped. Status 1: some of its processes remain.
if [ -d "$var/wrapper" ]; then
    status=0
    if command -v python3 >/dev/null 2>&1; then
        "$here/tools/lossless/stop.sh" >/dev/null 2>&1 || status=$?
    elif [ -x "$here/target/release/amui" ]; then
        "$here/target/release/amui" wrapper stop >/dev/null 2>&1 || status=$?
    fi
    if [ "$status" = 1 ]; then die "the lossless service did not stop: nothing was removed"; fi
fi

# The entry names the folder it starts: another copy's stays. (A folder with characters the entry
# has to escape is not compared.)
entry_is_ours() {
    case "$here" in *[!A-Za-z0-9_./\ -]*) return 0 ;; esac
    grep -qxF -e "Exec=$here/am-ui" -e "Exec=\"$here/am-ui\"" -- "$entry"
}
if [ -e "$entry" ] && ! entry_is_ours; then
    printf '\nThe application menu entry starts another copy of am-ui: it stays.\n'
else
    remove "$entry" "$icon"
    if command -v update-desktop-database >/dev/null 2>&1 && [ -d "$applications" ]; then
        update-desktop-database -q "$applications" 2>/dev/null || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1 && [ -e "$data_home/icons/hicolor/index.theme" ]; then
        gtk-update-icon-cache -q -t -f "$data_home/icons/hicolor" 2>/dev/null || true
    fi
    if command -v kbuildsycoca6 >/dev/null 2>&1; then kbuildsycoca6 >/dev/null 2>&1 || true; fi
fi

cavern=$here/tools/cavern-lab
downloader=$here/workspace/builds/music-downloader
remove "$here/target" "$var/build" "$var/toolchains" "$var/tmp" \
    "$here/crates/app/demo/covers" \
    "$cavern/probe/bin" "$cavern/probe/obj" \
    "$cavern/cavern-src/Cavern/bin" "$cavern/cavern-src/Cavern/obj" \
    "$cavern/cavern-src/Cavern.Format/bin" "$cavern/cavern-src/Cavern.Format/obj" \
    "$downloader/amdl-linux-target" "$downloader/amdl-patched"

if [ "$keep_data" = 0 ]; then
    remove "$var/cache" "$var/state" "$var/logs" "$var/wrapper" "$downloader/web-session"
    rmdir -- "$var" 2>/dev/null || true
    if [ -d "$var" ]; then printf '\n%s holds other files too: they stay.\n' "$var"; fi
fi

printf '\nDone. ./setup.sh installs am-ui again. To remove the source code too:\n  rm -rf %q\n' "$here"
