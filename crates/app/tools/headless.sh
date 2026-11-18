#!/bin/bash
# Run a program inside a private headless Wayland compositor: nothing appears on the real
# display, and the desktop session (its D-Bus, its settings, its windows) is not touched.
#
#   crates/app/tools/headless.sh [-s WIDTHxHEIGHT] PROGRAM [ARGUMENTS...]
#   crates/app/tools/headless.sh target/debug/am-ui --demo --page albums --screenshot /tmp/albums.png
#
# The compositor (mutter --headless, or KWin's virtual backend where there is no mutter) gets one
# virtual monitor (default 3840x2160, run at scale 2 like a real 4K display), its own D-Bus session
# and its own runtime folder; KWin also gets configuration folders of its own.
set -u
size=3840x2160
if [ "${1:-}" = -s ]; then size=$2; shift 2; fi
if [ $# -eq 0 ]; then sed -n '2,10p' "$0"; exit 2; fi
# a Unix socket path is at most 107 characters: keep the folder short
runtime=${AMUI_HEADLESS_DIR:-/tmp/amui-headless-$(id -u)}
mkdir -p "$runtime" && chmod 700 "$runtime" || exit 1
export XDG_RUNTIME_DIR=$runtime
# lets tools/fake-input.c drive the pointer (KWin only, and only this private compositor)
export KWIN_WAYLAND_NO_PERMISSION_CHECKS=1
# GIO without gvfs: a gvfs daemon of the private session would leave its FUSE mount behind in the
# runtime folder when the session ends
export GIO_USE_VFS=local GVFS_DISABLE_FUSE=1
unset WAYLAND_DISPLAY DISPLAY DBUS_SESSION_BUS_ADDRESS XAUTHORITY
exec dbus-run-session -- bash -c '
  name=$1; size=$2; shift 2
  log=$XDG_RUNTIME_DIR/compositor-$name.log
  if command -v mutter >/dev/null; then
    mutter --headless --no-x11 --wayland-display "$name" --virtual-monitor "$size" >"$log" 2>&1 &
  else
    width=${size%x*} height=${size#*x} scale=1
    # what mutter does by itself: a monitor this large runs at scale 2
    if [ "$width" -ge 2560 ] && [ "$height" -ge 1440 ]; then scale=2; fi
    state=$XDG_RUNTIME_DIR/kwin-$name
    mkdir -p "$state"
    XDG_CONFIG_HOME=$state XDG_DATA_HOME=$state XDG_CACHE_HOME=$state XDG_STATE_HOME=$state \
      kwin_wayland --virtual --no-lockscreen --no-global-shortcuts --no-kactivities \
        --socket "$name" --width $((width / scale)) --height $((height / scale)) --scale "$scale" >"$log" 2>&1 &
  fi
  compositor=$!
  for _ in $(seq 300); do [ -S "$XDG_RUNTIME_DIR/$name" ] && break; sleep 0.05; done
  if [ ! -S "$XDG_RUNTIME_DIR/$name" ]; then
    echo "headless.sh: the compositor did not start (see $log)" >&2
    kill "$compositor" 2>/dev/null
    exit 1
  fi
  WAYLAND_DISPLAY="$name" XDG_SESSION_TYPE=wayland GDK_BACKEND=wayland "$@"
  status=$?
  kill "$compositor" 2>/dev/null; wait "$compositor" 2>/dev/null
  rm -rf "$XDG_RUNTIME_DIR/$name" "$XDG_RUNTIME_DIR/$name.lock" "$log" "$XDG_RUNTIME_DIR/kwin-$name"
  exit $status
' headless "wl-$$" "$size" "$@" 2> >(grep -vE "dbus-daemon|xdg-desktop-portal|atk-bridge|AT-SPI|org.a11y|Broken pipe|connection to the bus|^$" >&2)
