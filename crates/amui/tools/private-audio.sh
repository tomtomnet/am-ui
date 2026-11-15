#!/bin/bash
# Run a program against a private audio server: a PipeWire daemon, WirePlumber and pipewire-pulse
# of their own, with a null sink as the only sink. Nothing is played on a real device, and the
# desktop session's audio server (its sockets, its D-Bus session, its settings) is not touched.
#
#   crates/amui/tools/private-audio.sh PROGRAM [ARGUMENTS...]
#   crates/amui/tools/private-audio.sh cargo test -p amui --offline --test output_audio -- --ignored --test-threads 1 --nocapture
#   crates/amui/tools/private-audio.sh pactl list short sinks
#
# The three programs run inside their own D-Bus session with runtime, configuration, state, data
# and cache folders of their own under /tmp/amui-audio-<uid>. They know no hardware: WirePlumber's
# device monitors are switched off and the daemon's network and desktop modules are not loaded; the
# sink "amui-null" (stereo, 48 kHz) is made in the daemon's configuration. Before PROGRAM runs, the
# server must list no device and exactly that sink as the only and default sink; otherwise nothing
# is run. PROGRAM finds only this server (XDG_RUNTIME_DIR, PIPEWIRE_RUNTIME_DIR, PULSE_SERVER) and
# can tell that it is the private one from AMUI_PRIVATE_AUDIO, which names its folder. The server
# is asked with its own tools (pactl, pw-cli) here and nowhere else. Everything is stopped and the
# folder removed when PROGRAM has ended.
set -u
if [ $# -eq 0 ]; then sed -n '2,18p' "$0"; exit 2; fi
for tool in dbus-run-session flock pipewire wireplumber pipewire-pulse pactl pw-cli; do
  command -v "$tool" >/dev/null || { echo "private-audio.sh: $tool is not installed" >&2; exit 1; }
done

# a Unix socket path is at most 107 characters: keep the folder short (and always below /tmp, since
# it is emptied first)
dir=/tmp/amui-audio-$(id -u)
# one server per folder: the lock is held by everything started here until it has ended
exec 9>"$dir.lock" || exit 1
flock -n 9 || { echo "private-audio.sh: a private audio server is already running in $dir" >&2; exit 1; }
rm -rf -- "$dir"
mkdir -p "$dir/config/pipewire/pipewire.conf.d" "$dir/config/pipewire/pipewire-pulse.conf.d" \
  "$dir/config/wireplumber/wireplumber.conf.d" "$dir/config/pulse" "$dir/state" "$dir/data" "$dir/cache" \
  "$dir/home" && chmod 700 "$dir" || exit 1

# the daemon: no network discovery, no desktop integration, one null sink
cat >"$dir/config/pipewire/pipewire.conf.d/10-private.conf" <<'EOF'
context.properties = {
    module.raop = false
    module.x11.bell = false
    module.jackdbus-detect = false
    module.portal = false
    default.clock.rate = 48000
    default.clock.allowed-rates = [ 48000 ]
}
context.objects = [
    { factory = adapter
        args = {
            factory.name     = support.null-audio-sink
            node.name        = amui-null
            node.description = "Private null sink"
            media.class      = Audio/Sink
            object.linger    = true
            audio.position   = [ FL FR ]
            audio.rate       = 48000
            monitor.channel-volumes = true
            monitor.passthrough = true
        }
    }
]
EOF
# the PulseAudio server: no stand-in sink of its own
cat >"$dir/config/pipewire/pipewire-pulse.conf.d/10-private.conf" <<'EOF'
pulse.properties = {
    pulse.cmd.always-sink = false
}
EOF
# the session manager: no device monitors
cat >"$dir/config/wireplumber/wireplumber.conf.d/10-no-hardware.conf" <<'EOF'
wireplumber.profiles = {
  main = {
    hardware.audio = disabled
    hardware.bluetooth = disabled
    hardware.video-capture = disabled
  }
}
EOF
# PulseAudio clients: this server, never one they would start or find themselves
printf 'autospawn = no\ndefault-server = unix:%s/pulse/native\n' "$dir" >"$dir/config/pulse/client.conf"

export XDG_RUNTIME_DIR=$dir XDG_CONFIG_HOME=$dir/config XDG_STATE_HOME=$dir/state XDG_DATA_HOME=$dir/data \
  XDG_CACHE_HOME=$dir/cache PIPEWIRE_RUNTIME_DIR=$dir
unset DISPLAY WAYLAND_DISPLAY XAUTHORITY DBUS_SESSION_BUS_ADDRESS \
  PIPEWIRE_REMOTE PIPEWIRE_CONFIG_DIR PIPEWIRE_CONFIG_NAME PIPEWIRE_CORE PIPEWIRE_NODE \
  PULSE_RUNTIME_PATH PULSE_STATE_PATH PULSE_CONFIG_PATH PULSE_SINK PULSE_SOURCE PULSE_PROP \
  WIREPLUMBER_CONFIG_DIR WIREPLUMBER_DATA_DIR
export PULSE_SERVER=unix:$dir/pulse/native PULSE_CLIENTCONFIG=$dir/config/pulse/client.conf \
  PULSE_COOKIE=$dir/config/pulse/cookie AMUI_PRIVATE_AUDIO=$dir
# no virtual file system service in the private session (the session manager would start one)
export GIO_USE_VFS=local GIO_USE_VOLUME_MONITOR=unix GVFS_DISABLE_FUSE=1

exec dbus-run-session -- bash -c '
  dir=$1; shift
  daemon= manager= pulse=
  stop() {
    trap - EXIT INT TERM
    kill $pulse $manager $daemon 2>/dev/null
    wait $pulse $manager $daemon 2>/dev/null
    rm -rf -- "$dir"
    rm -f -- "$dir.lock"
  }
  trap stop EXIT
  trap "exit 130" INT TERM
  fail() {
    echo "private-audio.sh: $1; the end of the logs in $dir:" >&2
    tail -n 5 "$dir"/*.log >&2
    exit 1
  }
  ask() { HOME=$dir/home "$@" 2>/dev/null; }
  HOME=$dir/home pipewire >"$dir/pipewire.log" 2>&1 & daemon=$!
  for _ in $(seq 200); do [ -S "$dir/pipewire-0" ] && break; sleep 0.05; done
  [ -S "$dir/pipewire-0" ] || fail "the PipeWire daemon did not start"
  HOME=$dir/home wireplumber >"$dir/wireplumber.log" 2>&1 & manager=$!
  HOME=$dir/home pipewire-pulse >"$dir/pipewire-pulse.log" 2>&1 & pulse=$!
  for _ in $(seq 200); do
    [ -S "$dir/pulse/native" ] && [ "$(ask pactl get-default-sink)" = amui-null ] && break
    sleep 0.05
  done
  [ -S "$dir/pulse/native" ] || fail "pipewire-pulse did not start"
  # what the server knows: no device, and the null sink as the only and default sink. pw-cli has to
  # see the sink, so that an empty device list cannot come from a failed connection.
  nodes=$(ask pw-cli ls Node) || fail "pw-cli cannot reach the private daemon"
  case $nodes in *"node.name = \"amui-null\""*) ;; *) fail "pw-cli does not see the null sink";; esac
  devices=$(ask pw-cli ls Device | grep -c "PipeWire:Interface:Device")
  sinks=$(ask pactl list short sinks | cut -f2 | tr "\n" " ")
  default=$(ask pactl get-default-sink)
  [ "$devices" = 0 ] || fail "the private server lists $devices device(s)"
  [ "$sinks" = "amui-null " ] || fail "the private server lists these sinks: ${sinks:-none}"
  [ "$default" = amui-null ] || fail "the default sink is ${default:-not set}"
  "$@"
' private-audio "$dir" "$@"
