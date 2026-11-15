#!/usr/bin/env python3
"""Generate and tune the Atmos 9.1.6 -> stereo downmix (PipeWire filter-chain).

  downmix.py write  [preset] [--group value ...]   write pipewire/atmos-916-downmix.conf, the repo's example
                                                   (EQ flat; no preset or value given: a new machine's config)
  downmix.py config                                print this machine's config: its saved downmix settings and
                                                   EQ (what the setup script installs)
  downmix.py apply  [preset] [--group value ...]   change the running filter live
  downmix.py show   [preset] [--group value ...]   print the resulting matrix

Presets:
  peqdb  peqdb Downmix Renderer, from its renderer log: every side channel at
         1.0, centre -3 dB, LFE x2.26464 (+7.1 dB) after a 4th-order 125 Hz
         Butterworth low-pass, other channels delayed to stay aligned with it.
  itu    "Default": conservative broadcast-style fold-down (PipeWire's own
         defaults): surrounds -3 dB, wides -1.25 dB, heights -6 dB, LFE 0.354.
  apple  macOS's own fold-down (measured): everything but the front pair at
         -3 dB, LFE dropped.

Gains are linear amplitude factors; --preamp is in dB. Example:
  downmix.py apply peqdb --preamp -6 --top-front 0.7

After the downmix comes a headphone EQ (like peqdb's renderer): fixed slots of PipeWire
biquads with live controls, filled from the active preset in
~/.config/applemusic-bridge/eq.json (managed by the bridge's web app; `install` bakes the
current values into this machine's installed config so they survive PipeWire restarts).

Each machine has its own config (its settings and EQ), never the repo's example.
"""
import argparse, json, math, os, re, subprocess, sys, tempfile, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
CONF = os.path.join(HERE, "..", "pipewire", "atmos-916-downmix.conf")
# a drop-in of the downmix's own PipeWire process (applemusic-downmix.service, pipewire/applemusic-downmix.conf;
# until 2026-10-26 ~/.config/pipewire/pipewire.conf.d/, inside the daemon, where the desktop's mixer hid it)
INSTALLED = os.path.expanduser("~/.config/pipewire/applemusic-downmix.conf.d/atmos-916-downmix.conf")
NODE = "atmos_916_downmix"
# this machine's downmix settings (the meter page's preset, preamp, loudness match; not in git) and their defaults
STATE_FILE = os.path.join(HERE, "..", "pipewire", "downmix-state.json")
DEFAULT_STATE = {"preset": "peqdb", "preamp": -11.0, "match": True}

# per-side gain groups, in the order they enter each side's mixer (In 1..8)
GROUPS = ["front", "center", "surround", "rear", "wide", "top_front", "top_middle", "top_rear"]
# input channel -> (mixer side, mixer input); FC and LFE are routed separately
LEFT = ["FL", None, "SL", "RL", "FLW", "TFL", "TSL", "TRL"]
RIGHT = ["FR", None, "SR", "RR", "FRW", "TFR", "TSR", "TRR"]
POSITIONS = "FL FR FC LFE SL SR RL RR FLW FRW TFL TFR TSL TSR TRL TRR".split()
BW4_Q = (1 / (2 * math.cos(math.pi / 8)), 1 / (2 * math.cos(3 * math.pi / 8)))

# EQ slots per side: filters of one type commute, so a preset's filters fill the slots of
# their type in any order; unused slots stay at 0 dB (pass-through)
EQ_SLOTS = (("LSC", "bq_lowshelf", 4), ("PK", "bq_peaking", 24), ("HSC", "bq_highshelf", 4))
EQ_STATE = os.path.expanduser("~/.config/applemusic-bridge/eq.json")

PRESETS = {
    "itu": dict(front=1.0, center=0.70710678, surround=0.70710678, rear=0.70710678,
                wide=0.8660, top_front=0.5, top_middle=0.5, top_rear=0.5,
                lfe=0.35355339, lfe_lowpass=0.0, lfe_delay=0.0, preamp=0.0),
    "peqdb": dict(front=1.0, center=0.70710678, surround=1.0, rear=1.0, wide=1.0,
                  top_front=1.0, top_middle=1.0, top_rear=1.0,
                  lfe=2.26464, lfe_lowpass=125.0, lfe_delay="auto", preamp=0.0),
    # macOS's own fold-down, measured 2026-10-10 by switching the Roc device to a stereo
    # speaker layout mid-song and fitting the result against the 16-channel render
    # (notes/analysis/apple-downmix): every channel but the front pair at -3 dB into its
    # side, LFE dropped. Apple Music's streamed "-downmix" variant is never picked on macOS.
    "apple": dict(front=1.0, center=0.70710678, surround=0.70710678, rear=0.70710678,
                  wide=0.70710678, top_front=0.70710678, top_middle=0.70710678, top_rear=0.70710678,
                  lfe=0.0, lfe_lowpass=0.0, lfe_delay=0.0, preamp=0.0),
}
LABELS = {"itu": "Default", "peqdb": "PEQdB", "apple": "Apple"}
# extra gain that makes a preset as loud as peqdb on the same music, for fair A/B
# switching; measured on one reference track: peqdb -15.4 LUFS, itu -17.0 LUFS at 0 dB preamp
# (apple: 0.1 dB quieter than itu on the same capture)
LOUDNESS_OFFSET = {"peqdb": 0.0, "itu": 1.6, "apple": 1.7}


def resolve(args):
    p = dict(PRESETS[args.preset])
    for k in list(p):
        v = getattr(args, k, None)
        if v is not None:
            p[k] = v
    if p["lfe_delay"] == "auto":
        # group delay at DC of the 4th-order Butterworth, + half a sample at 48 kHz
        p["lfe_delay"] = (sum(1 / q for q in BW4_Q) / (2 * math.pi * p["lfe_lowpass"]) + 0.5 / 48000
                          if p["lfe_lowpass"] > 0 else 0.0)
    return p


def params_for(state):
    """preset parameters with the preamp (+ loudness matching) applied"""
    offset = LOUDNESS_OFFSET.get(state["preset"], 0.0) if state["match"] else 0.0
    return resolve(argparse.Namespace(preset=state["preset"], preamp=state["preamp"] + offset))


def saved_state(path=None):
    """This machine's downmix settings (STATE_FILE, written by atmosmeter's Controller), defaults filled in."""
    st = dict(DEFAULT_STATE)
    try:
        with open(path or STATE_FILE) as f:
            st.update({k: v for k, v in json.load(f).items() if k in DEFAULT_STATE})
    except (OSError, ValueError):
        pass
    return st


def parse_eq(text):
    """EqualizerAPO / AutoEQ / peqdb text -> (preamp dB, [{type, freq, gain, q}]).
    Raises ValueError("line N: ...") on anything it doesn't understand."""
    import re
    preamp, filters = 0.0, []
    for n, raw in enumerate(text.splitlines(), 1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        m = re.fullmatch(r"(?i)preamp\s*:\s*([-+]?\d+(?:\.\d+)?)\s*db", line)
        if m:
            preamp = float(m.group(1))
            continue
        m = re.fullmatch(r"(?i)filter\s*\d*\s*:\s*(on|off)\s+(pk|peq|lsc|ls|hsc|hs)\s+fc\s+([\d.]+)\s*hz"
                         r"\s+gain\s+([-+]?[\d.]+)\s*db(?:\s+q\s+([\d.]+))?", line)
        if not m:
            raise ValueError(f"line {n}: not a Preamp or Filter line: {raw.strip()[:60]}")
        if m.group(1).lower() == "off":
            continue
        kind = {"pk": "PK", "peq": "PK", "lsc": "LSC", "ls": "LSC", "hsc": "HSC", "hs": "HSC"}[m.group(2).lower()]
        freq, gain, q = float(m.group(3)), float(m.group(4)), float(m.group(5) or 0.707)
        if not 10 <= freq <= 23000 or not -30 <= gain <= 30 or not 0.05 <= q <= 20:
            raise ValueError(f"line {n}: values out of range")
        filters.append({"type": kind, "freq": freq, "gain": gain, "q": q})
    if not -40 <= preamp <= 20:
        raise ValueError("preamp out of range")
    for kind, _, slots in EQ_SLOTS:
        if sum(f["type"] == kind for f in filters) > slots:
            raise ValueError(f"too many {kind} filters (max {slots})")
    return preamp, filters


def eq_controls(preamp=0.0, filters=()):
    """filter-graph control values for the EQ slots of both sides"""
    c = {}
    for side in ("l", "r"):
        c[f"eqgain_{side}:Mult"] = 10 ** (preamp / 20)
        for kind, _, slots in EQ_SLOTS:
            mine = [f for f in filters if f["type"] == kind]
            for i in range(slots):
                f = mine[i] if i < len(mine) else {"freq": 1000.0, "gain": 0.0, "q": 0.707}
                c[f"eq_{side}_{kind.lower()}{i + 1}:Freq"] = f["freq"]
                c[f"eq_{side}_{kind.lower()}{i + 1}:Q"] = f["q"]
                c[f"eq_{side}_{kind.lower()}{i + 1}:Gain"] = f["gain"]
    return c


def volume_db(percent):
    """The clone's volume (0-100) as a gain: cubic like PipeWire's (50 % = -18 dB), 0 = silence."""
    v = max(0.0, min(100.0, float(percent))) / 100
    return 60 * math.log10(v) if v > 0 else -math.inf


def eq_current():
    """EQ control values for the state saved by the web app: the active preset (flat when off/unset), and on the
    gain stage its preamp plus the clone's saved volume, so a downmix that starts again plays at that volume (the
    song's Sound Check trim, never saved, the bridge sets again)."""
    try:
        with open(EQ_STATE) as f:
            st = json.load(f)
    except (OSError, ValueError):
        return eq_controls()
    try:
        vol = volume_db(st.get("volume", 100))
    except (TypeError, ValueError):
        vol = 0.0
    if st.get("enabled"):
        pre = next((x for x in st.get("presets", []) if x.get("id") == st.get("active")), None)
        if pre:
            return eq_controls(pre.get("preamp", 0.0) + vol, pre.get("filters", []))
    return eq_controls(vol)


def controls(p):
    """filter-graph control values for a parameter set"""
    c = {}
    for side in ("dry_l", "dry_r"):
        for i, g in enumerate(GROUPS):
            c[f"{side}:Gain {i + 1}"] = p[g]
    # a cutoff at/above Nyquist turns PipeWire's biquad into a pass-through
    freq = p["lfe_lowpass"] if p["lfe_lowpass"] > 0 else 1e6
    c["lfe_lp1:Freq"] = c["lfe_lp2:Freq"] = freq
    c["lfe_lp1:Q"], c["lfe_lp2:Q"] = BW4_Q
    c["dly_l:Delay (s)"] = c["dly_r:Delay (s)"] = p["lfe_delay"]
    c["out_l:Gain 2"] = c["out_r:Gain 2"] = p["lfe"]
    c["preamp_l:Mult"] = c["preamp_r:Mult"] = 10 ** (p["preamp"] / 20)
    return c


def show(p):
    print(f"{'input':>6}  {'L':>8} {'R':>8}")
    for ch in POSITIONS:
        if ch == "FC":
            l = r = p["center"]
        elif ch == "LFE":
            l = r = p["lfe"]
        elif ch in LEFT:
            l, r = p[GROUPS[LEFT.index(ch)]], 0.0
        else:
            l, r = 0.0, p[GROUPS[RIGHT.index(ch)]]
        print(f"{ch:>6}  {l:8.4f} {r:8.4f}")
    lp = f"{p['lfe_lowpass']:g} Hz 4th-order Butterworth" if p["lfe_lowpass"] > 0 else "off"
    print(f"LFE low-pass: {lp}, dry delay {p['lfe_delay'] * 1000:.3f} ms, preamp {p['preamp']:+.1f} dB")


def node_value(v):
    return f"{v:.8g}"


def render(p, eqc):
    """The config's text for downmix parameters p and EQ control values eqc (eq_controls())."""
    c = controls(p)
    def ctl(name, keys):
        return " ".join(f'"{k.split(":", 1)[1]}" = {node_value(c[k])}' for k in keys if k.startswith(name + ":"))
    mix_ctl = {s: ctl(s, [f"{s}:Gain {i + 1}" for i in range(8)]) for s in ("dry_l", "dry_r")}
    def node(kind, name, keys):
        vals = " ".join(f'"{k.split(":", 1)[1]}" = {node_value(eqc[k])}' for k in keys)
        return f"{{ type = builtin name = {name} label = {kind} control = {{ {vals} }} }}"
    eq_nodes, eq_links, eq_last = [], [], {}
    for side in ("l", "r"):
        prev = f"eqgain_{side}"
        eq_nodes.append(node("linear", prev, [f"{prev}:Mult"]))
        eq_links.append(f'{{ output = "preamp_{side}:Out" input = "{prev}:In" }}')
        for kind, label, slots in EQ_SLOTS:
            for i in range(1, slots + 1):
                name = f"eq_{side}_{kind.lower()}{i}"
                eq_nodes.append(node(label, name, [f"{name}:Freq", f"{name}:Q", f"{name}:Gain"]))
                eq_links.append(f'{{ output = "{prev}:Out" input = "{name}:In" }}')
                prev = name
        eq_last[side] = prev
    pad = "\n                    "
    inputs = []
    for ch in POSITIONS:
        if ch == "FC":
            inputs.append('"c:In"')
        elif ch == "LFE":
            inputs.append('"lfe_lp1:In"')
        elif ch in LEFT:
            inputs.append(f'"dry_l:In {LEFT.index(ch) + 1}"')
        else:
            inputs.append(f'"dry_r:In {RIGHT.index(ch) + 1}"')
    text = f"""# Dolby Atmos 9.1.6 -> stereo downmix sink. GENERATED by tools/downmix.py -
# edit presets/values there, not here.
#
# Creates a 16-channel sink "Atmos 9.1.6 Downmix" whose output goes to your
# default stereo device. Its class is Audio/Sink/Virtual, not Audio/Sink: PipeWire's
# Pulse layer then doesn't list it, so the desktop's sound settings can't offer it as an
# output and WirePlumber never makes it the default (a 16-channel stream would land on the
# headphones as plain FL/FR). Native tools still see it by name.
# Channel order as in macOS Audio MIDI Setup 9.1.6
# ("channels 1-16 ascending"):
#    1 L    2 R    3 C    4 LFE   5 Ls   6 Rs   7 Lrs   8 Rrs
#    9 Lw  10 Rw  11 Ltf 12 Rtf  13 Ltm 14 Rtm  15 Ltr  16 Rtr
#  = {" ".join(POSITIONS)}
# (PipeWire's own remix would silently drop FLW/FRW/TSL/TSR.)
#
# Signal flow per side: 8 dry channels (+ centre) -> mixer -> delay ->
# + LFE (low-passed, shared by both sides) -> preamp -> headphone EQ (gain + biquads).
# All gains, the LFE filter, the delay and the preamp are live controls:
# change them with `tools/downmix.py apply ...` without restarting PipeWire.
#
# Each machine has its own, with its own settings and EQ (`tools/downmix.py config`): the setup script
# installs it as ~/.config/pipewire/applemusic-downmix.conf.d/atmos-916-downmix.conf and the bridge rewrites it
# when they change. pipewire/atmos-916-downmix.conf in the repo is an example (`downmix.py write`, the EQ flat).
# The drop-in belongs to the downmix's own PipeWire client (applemusic-downmix.service,
# pipewire/applemusic-downmix.conf), which the desktop's volume mixer shows as "Apple Music".
# Reload: systemctl --user restart applemusic-downmix

context.modules = [
    {{ name = libpipewire-module-filter-chain
        # a broken edit here should not take the whole PipeWire daemon down
        flags = [ nofail ]
        args = {{
            node.description = "Atmos 9.1.6 Downmix"
            media.name       = "Atmos 9.1.6 Downmix"
            filter.graph = {{
                nodes = [
                    {{ type = builtin name = c label = copy }}
                    {{ type = builtin name = dry_l label = mixer control = {{ {mix_ctl["dry_l"]} }} }}
                    {{ type = builtin name = dry_r label = mixer control = {{ {mix_ctl["dry_r"]} }} }}
                    {{ type = builtin name = lfe_lp1 label = bq_lowpass control = {{ {ctl("lfe_lp1", ["lfe_lp1:Freq", "lfe_lp1:Q"])} }} }}
                    {{ type = builtin name = lfe_lp2 label = bq_lowpass control = {{ {ctl("lfe_lp2", ["lfe_lp2:Freq", "lfe_lp2:Q"])} }} }}
                    {{ type = builtin name = dly_l label = delay config = {{ "max-delay" = 0.1 }} control = {{ {ctl("dly_l", ["dly_l:Delay (s)"])} }} }}
                    {{ type = builtin name = dly_r label = delay config = {{ "max-delay" = 0.1 }} control = {{ {ctl("dly_r", ["dly_r:Delay (s)"])} }} }}
                    {{ type = builtin name = out_l label = mixer control = {{ "Gain 1" = 1 {ctl("out_l", ["out_l:Gain 2"])} }} }}
                    {{ type = builtin name = out_r label = mixer control = {{ "Gain 1" = 1 {ctl("out_r", ["out_r:Gain 2"])} }} }}
                    {{ type = builtin name = preamp_l label = linear control = {{ {ctl("preamp_l", ["preamp_l:Mult"])} }} }}
                    {{ type = builtin name = preamp_r label = linear control = {{ {ctl("preamp_r", ["preamp_r:Mult"])} }} }}
                    # headphone EQ: per side a gain (the preset's preamp), then the
                    # biquad slots (shelves and peaking filters, live controls)
                    {pad.join(eq_nodes)}
                ]
                links = [
                    {{ output = "c:Out"        input = "dry_l:In 2" }}
                    {{ output = "c:Out"        input = "dry_r:In 2" }}
                    {{ output = "dry_l:Out"    input = "dly_l:In" }}
                    {{ output = "dry_r:Out"    input = "dly_r:In" }}
                    {{ output = "dly_l:Out"    input = "out_l:In 1" }}
                    {{ output = "dly_r:Out"    input = "out_r:In 1" }}
                    {{ output = "lfe_lp1:Out"  input = "lfe_lp2:In" }}
                    {{ output = "lfe_lp2:Out"  input = "out_l:In 2" }}
                    {{ output = "lfe_lp2:Out"  input = "out_r:In 2" }}
                    {{ output = "out_l:Out"    input = "preamp_l:In" }}
                    {{ output = "out_r:Out"    input = "preamp_r:In" }}
                    {pad.join(eq_links)}
                ]
                # one entry per channel of capture.props audio.position, in order
                inputs = [ {" ".join(inputs)} ]
                outputs = [ "{eq_last['l']}:Out" "{eq_last['r']}:Out" ]
            }}
            capture.props = {{
                node.name      = "{NODE}"
                media.class    = "Audio/Sink/Virtual"
                audio.channels = 16
                audio.position = [ {" ".join(POSITIONS)} ]
            }}
            playback.props = {{
                node.name         = "{NODE}.output"
                audio.channels    = 2
                audio.position    = [ FL FR ]
                stream.dont-remix = true
                node.passive      = true
                # not virtual (filter-chain's default): PipeWire's pulse layer then gives the stream its client
                # ("Apple Music", applemusic-downmix.service), and the desktop's volume mixer shows it as an app
                node.virtual      = false
                # its name and icon there (the mixer looks for the icon in the stream's own properties)
                application.name      = "Apple Music"
                application.icon_name = "applemusic-bridge"
            }}
        }}
    }}
]
"""
    return text


def eq_from(eq_file=None):
    """EQ control values: an EqualizerAPO/AutoEQ file's, else the web app's saved EQ (eq_current)."""
    if not eq_file:
        return eq_current()
    with open(eq_file) as f:
        return eq_controls(*parse_eq(f.read()))


def write(p, eq_file=None):
    """The repo's example config (CONF): parameters p, the EQ flat (or eq_file's). Never this machine's EQ: the
    example travels to every machine with git."""
    with open(CONF, "w") as f:
        f.write(render(p, eq_controls() if not eq_file else eq_from(eq_file)))
    print(f"wrote {os.path.normpath(CONF)}")


def machine_config():
    """This machine's config: its saved downmix settings (saved_state) and the EQ the web app saved (eq.json).
    What the setup script installs (`downmix.py config`); install() keeps it current."""
    return render(params_for(saved_state()), eq_current())


_install_lock = threading.Lock()      # the bridge's EQ and the meter's downmix changes install from two threads


def install(p, eq_file=None):
    """Rewrite this machine's installed config (where the setup script put one) with parameters p and the saved EQ, so
    both survive a PipeWire restart (the running filter is changed with apply() / apply_controls()). True when it
    changed. The repo's example (CONF) is left alone. One at a time, through a temp file of its own (not *.conf:
    PipeWire never reads it) renamed over the config."""
    with _install_lock:
        if not os.path.exists(INSTALLED):
            return False
        text = render(p, eq_from(eq_file))
        try:
            with open(INSTALLED) as f:
                if f.read() == text:
                    return False
        except OSError:
            pass
        fd, tmp = tempfile.mkstemp(dir=os.path.dirname(INSTALLED), prefix=".atmos-916-downmix.", suffix=".tmp")
        try:
            with os.fdopen(fd, "w") as f:
                f.write(text)
            os.chmod(tmp, 0o644)
            os.replace(tmp, INSTALLED)
        except BaseException:
            try:
                os.unlink(tmp)
            except OSError:
                pass
            raise
        return True


_node_ids = {}          # node name -> (id, monotonic time it was looked up, process_marker() then)
NODE_ID_TTL = 5.0       # a volume drag sends bursts: one pw-dump (~30 ms) per burst, not per step


def set_props(nid, params):
    """pw-cli set-param Props on node nid; False when it failed. pw-cli exits 0 then too, printing
    'Error: "set-param: unknown global ..."' (a node that is gone)."""
    r = subprocess.run(["pw-cli", "set-param", str(nid), "Props", f"{{ params = [ {params} ] }}"],
                       capture_output=True, text=True)
    return r.returncode == 0 and not any(line.startswith("Error") for line in (r.stdout + "\n" + r.stderr).splitlines())


def apply_controls(c, node=NODE):
    """set filter-graph controls on the running sink (no restart). The node's id is kept NODE_ID_TTL s, and only
    while the downmix's process stays the same (process_marker): PipeWire gives a gone node's id to new objects."""
    params = " ".join(f'"{k}" {node_value(v)}' for k, v in c.items())
    cached = _node_ids.get(node)
    if cached and time.monotonic() - cached[1] < NODE_ID_TTL and cached[2] == process_marker():
        if set_props(cached[0], params):
            return cached[0]
    dump = json.loads(subprocess.run(["pw-dump"], capture_output=True, text=True, check=True).stdout)
    ids = [o["id"] for o in dump if o.get("type") == "PipeWire:Interface:Node"
           and o.get("info", {}).get("props", {}).get("node.name") == node]
    if not ids:
        _node_ids.pop(node, None)
        raise RuntimeError(f"node '{node}' not found - is the downmix loaded?")
    if not set_props(ids[0], params):
        _node_ids.pop(node, None)
        raise RuntimeError(f"could not set the controls of node {ids[0]} ({node})")
    _node_ids[node] = (ids[0], time.monotonic(), process_marker())
    return ids[0]


def node_id(node=NODE):
    """The running sink's node id (pw-cli ls: ~6 ms), or None while it isn't there."""
    key = node_key(node)
    return key[0] if key else None


def node_key(node=NODE):
    """(id, object.serial) of the running sink, or None while it isn't there. The serial tells a new node from the
    old one: PipeWire gives a removed object's id to the next ones, never its serial."""
    out = subprocess.run(["pw-cli", "ls", "Node"], capture_output=True, text=True, timeout=10).stdout
    cur, serial = None, None
    for line in out.splitlines():
        s = line.strip()
        if s.startswith("id ") and "PipeWire:Interface:Node" in s:
            cur, serial = int(s.split()[1].rstrip(",")), None
        elif s.startswith("object.serial = "):
            serial = s.split("=", 1)[1].strip().strip('"')
        elif cur is not None and s == f'node.name = "{node}"':
            return cur, serial
    return None


def forget_node(node=NODE):
    """Drop the cached node id (PipeWire hands a removed node's id to new objects)."""
    _node_ids.pop(node, None)


_UNSET = object()
_cgroup_procs = _UNSET


def _downmix_cgroup(unit="applemusic-downmix.service"):
    """cgroup.procs of the downmix's systemd user unit (pipewire/applemusic-downmix.service: session.slice), found
    from this process's own cgroup (.../user@UID.service/...); None outside a systemd user session."""
    try:
        with open("/proc/self/cgroup") as f:
            path = f.read().split("::", 1)[1].strip()
    except (OSError, IndexError):
        return None
    m = re.match(r"/user\.slice/user-\d+\.slice/user@\d+\.service", path)
    return f"/sys/fs/cgroup{m.group(0)}/session.slice/{unit}/cgroup.procs" if m else None


def process_marker():
    """The downmix process's pid(s) (its unit's cgroup.procs: a sysfs read, microseconds), None while it isn't
    running or can't be seen (an older setup ran the downmix inside the PipeWire daemon). Changes when the process
    starts again, so a watcher sees a new downmix within a second without listing nodes (node_key: ~4 ms of CPU)."""
    global _cgroup_procs
    if _cgroup_procs is _UNSET:
        _cgroup_procs = _downmix_cgroup()
    if not _cgroup_procs:
        return None
    try:
        with open(_cgroup_procs) as f:
            return f.read().strip() or None
    except OSError:
        return None


def apply(p, node):
    dump = json.loads(subprocess.run(["pw-dump"], capture_output=True, text=True, check=True).stdout)
    ids = [o["id"] for o in dump if o.get("type") == "PipeWire:Interface:Node"
           and o.get("info", {}).get("props", {}).get("node.name") == node]
    if not ids:
        sys.exit(f"node '{node}' not found - is the downmix loaded?")
    params = " ".join(f'"{k}" {node_value(v)}' for k, v in controls(p).items())
    if not set_props(ids[0], params):
        sys.exit(f"could not set the controls of node {ids[0]} ({node})")
    print(f"applied to node {ids[0]} ({node})")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("action", choices=["write", "config", "apply", "show"])
    ap.add_argument("preset", nargs="?", choices=sorted(PRESETS), help="default: peqdb")
    for g in GROUPS + ["lfe"]:
        ap.add_argument("--" + g.replace("_", "-"), dest=g, type=float, metavar="GAIN")
    ap.add_argument("--lfe-lowpass", dest="lfe_lowpass", type=float, metavar="HZ", help="0 = off")
    ap.add_argument("--lfe-delay", dest="lfe_delay", type=float, metavar="SECONDS",
                    help="dry-path delay (default: matches the LFE filter)")
    ap.add_argument("--preamp", type=float, metavar="DB")
    ap.add_argument("--eq-file", help="AutoEQ/EqualizerAPO parametric EQ file (write only)")
    ap.add_argument("--node", default=NODE, help="sink node.name to apply to")
    args = ap.parse_args()
    if args.action == "config":
        sys.stdout.write(machine_config())
        return
    given = args.preset is not None or any(getattr(args, k) is not None
                                           for k in GROUPS + ["lfe", "lfe_lowpass", "lfe_delay", "preamp"])
    args.preset = args.preset or "peqdb"
    # the example, with nothing given: what a machine with no settings saved yet gets
    p = resolve(args) if given or args.action != "write" else params_for(DEFAULT_STATE)
    if args.action == "show":
        show(p)
    elif args.action == "write":
        write(p, args.eq_file)
        show(p)
    else:
        apply(p, args.node)
        show(p)


if __name__ == "__main__":
    main()
