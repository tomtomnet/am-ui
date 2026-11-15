#!/usr/bin/env python3
"""Reference values for the Rust ports of amui/downmix.py, amui/eq.py and workspace/tools/downmix.py.

    python3 crates/amui/tests/reference/generate.py > crates/amui/tests/reference/python.json

Everything here is computed by the Python implementation: the tool's tables and pure functions
(resolve, controls, show, node_value, parse_eq, eq_controls), the engine's pure functions (profiles,
expected_controls, effective_preamp, parse, response, curve, ...) and state files written into a
temporary folder. Nothing is applied: no pw-cli, no pw-dump, no sink, and no file outside the
temporary folder is read or written. The tests in crates/amui/src/downmix.rs and eq.rs compare the
Rust results with python.json.

Floats are written as their Python repr in strings, so a reader gets every bit back.
"""
from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
import shutil
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
SCRATCH = Path(tempfile.mkdtemp(prefix="amui-reference-"))
os.environ["AMUI_WORKSPACE"] = str(ROOT / "workspace")  # the bundled tool, whatever else a machine has
os.environ["AMUI_VAR"] = str(SCRATCH / "var")
sys.dont_write_bytecode = True
sys.path.insert(0, str(ROOT))

from amui import cache, downmix, eq  # noqa: E402

tool = downmix.workspace_module()
NODE_ID = 38

APO_TEXT = """# AutoEQ-style export (comments and blank lines are skipped)
Preamp: -6.2 dB

Filter 1: ON PK Fc 105 Hz Gain -2.6 dB Q 0.70
Filter 2: ON LSC Fc 60 Hz Gain 3.5 dB Q 0.71
Filter 3: OFF PK Fc 2000 Hz Gain -9 dB Q 4.0
Filter 4: ON HSC Fc 10000 Hz Gain 3.0 dB Q 0.70
Filter 5: ON PEQ Fc 3200 Hz Gain 4 dB Q 2
"""
PEQDB_TEXT = "Preamp: -10.4 dB\n" + "\n".join(
    f"Filter {i + 1}: ON PK Fc {f} Hz Gain {g} dB Q {q}" for i, (f, g, q) in enumerate(
        [(20, -3.9, 0.5), (105, 2.1, 0.8), (210, -1.2, 1.4), (640, 1.0, 2.0), (1500, -2.4, 1.1), (2600, 3.3, 1.6),
         (3900, -1.7, 2.5), (5200, 2.0, 3.0), (6700, -3.1, 2.2), (8100, 1.4, 1.9), (9600, -0.8, 1.2), (12000, 2.5, 1.0),
         (16000, -1.1, 0.9)])) + "\nFilter 14: ON LS Fc 80 Hz Gain 1.5 dB Q 0.7\nFilter 15: ON HS Fc 11000 Hz Gain -2 dB Q 0.7\n"
NO_Q_TEXT = "Filter: ON PK Fc 500 Hz Gain 1 dB"
EDGES_TEXT = ("Preamp: 20 dB\nFilter 1: ON LS Fc 10 Hz Gain 30 dB Q 0.05\nFilter 2: ON HS Fc 23000 Hz Gain -30 dB Q 20\n"
              "Filter 3: ON PK Fc 23000 Hz Gain 30 dB Q 0.05")

PARSE_TEXTS = [
    APO_TEXT, PEQDB_TEXT, NO_Q_TEXT, "",
    # the errors of tests/test_eq.py
    "Preamp: -6 dB\nFilter 1: ON PK Fc 1000 Hz Gain 6 dB Q 1\nDevice: Headphones\n",
    "Filter 1: ON PK Fc 1000 Hz Gain 60 dB Q 1",
    "Preamp: -50 dB",
    "\n".join(f"Filter {i}: ON LSC Fc 100 Hz Gain 1 dB Q 0.7" for i in range(5)),
    "\n".join(f"Filter {i}: ON PK Fc {100 + i} Hz Gain 1 dB Q 1" for i in range(25)),
    "\n".join(f"Filter {i}: ON PK Fc {100 + i} Hz Gain 1 dB Q 1" for i in range(24)),
    "\n".join(f"Filter {i}: ON HS Fc 9000 Hz Gain 1 dB" for i in range(5)),
    # spelling
    "preamp:-3db", "PREAMP : +3 DB", "Preamp: 3.5 dB", "Preamp: -0 dB", "Preamp: .5 dB", "Preamp: 3. dB", "Preamp: 1e1 dB",
    "Preamp: -6.2 dB # louder masters", "  Preamp: -1 dB  ", "#", " # only a comment", "Preamp: -1 dB\nPreamp: -2 dB",
    "Filter 1: ON PK Fc 100 Hz Gain 1 dB Q 1\nPreamp: -4 dB",
    "FILTER 3: ON LS FC 100 HZ GAIN 1 DB Q 0.5", "filter12:on peq fc 100hz gain +1.5db q .5",
    "Filter 1 : ON HS Fc 100Hz Gain 1dB Q 2", "Filter 1: ON PK Fc 100 Hz Gain 1 dB Q 1 extra",
    "Filter 1: ON PK Fc 1e3 Hz Gain 1 dB Q 1", "Filter 1: ON BP Fc 100 Hz Gain 1 dB Q 1",
    "Filter 1: ON PK Fc 100 Hz Gain 1 dB Q", "Filter 1: ONPK Fc 100 Hz Gain 1 dB",
    "Filter 1: OFF PK Fc 1.2.3 Hz Gain 1 dB", "Filter 1: ON PK Fc 1.2.3 Hz Gain 1 dB",
    "Filter 1: ON PK Fc 100 Hz Gain -. dB", "Filter 1: ON PK Fc 100 Hz Gain 1 dB Q .",
    "Filter 1: ON PK Fc 5. Hz Gain -.5 dB Q 5.", "Preamp: 1 dB\nFilter 1: ON PK Fc . Hz Gain 1 dB",
    # ranges
    "Preamp: -40 dB", "Preamp: 20 dB", "Preamp: 20.0001 dB", "Preamp: -40.5 dB",
    "Filter 1: ON PK Fc 10 Hz Gain -30 dB Q 0.05", "Filter 1: ON PK Fc 23000 Hz Gain 30 dB Q 20",
    "Filter 1: ON PK Fc 9.99 Hz Gain 1 dB Q 1", "Filter 1: ON PK Fc 23000.1 Hz Gain 1 dB Q 1",
    "Filter 1: ON PK Fc 100 Hz Gain -30.01 dB Q 1", "Filter 1: ON PK Fc 100 Hz Gain 1 dB Q 0.04",
    "Filter 1: ON PK Fc 100 Hz Gain 1 dB Q 20.5", "\nFilter 1: ON PK Fc 5 Hz Gain 1 dB Q 1",
    "Filter 1: ON PK Fc " + "9" * 400 + " Hz Gain 1 dB Q 1",
    # line ends and blanks
    "Preamp: -1 dB\r\nFilter 1: ON PK Fc 100 Hz Gain 1 dB Q 1\r\nnonsense\r\n", "Preamp: -1 dB\rnonsense",
    "Preamp: -1 dB\x0cnonsense", "Preamp: -1 dB\x0bnonsense", "Preamp: -1 dB\x1cnonsense", "Preamp: -1 dB\x1dnonsense",
    "Preamp: -1 dB\x1enonsense", "Preamp: -1 dB\x85nonsense", "Preamp: -1 dB nonsense", "Preamp: -1 dB nonsense",
    "\n\n\nnonsense", "Preamp:\t-1\tdB", "Preamp:\xa0-1\xa0dB", "\x1fPreamp:\x1f-1\x1fdB\x1f", "Preamp:　-1 dB",
    "Filter\t1:\tON\tPK\tFc\t100\tHz\tGain\t1\tdB\tQ\t1", "﻿Preamp: -1 dB",
    # the message keeps 60 characters of the line
    "   " + "x" * 80 + "   ", "Device: " + "é" * 70, "nonsense # with a comment",
    # letters and digits beyond ASCII that Python's re and float() take
    "Fılter 1: ON PK Fc 100 Hz Gaın 1 dB", "FİLTER 1: ON PK Fc 100 Hz GAİN 1 dB",
    "Filter 1: ON PK Fc 100 Hz Gain 1 dB", "Preamp: -３ dB",
    "Filter ٣: ON PK Fc １０５ Hz Gain ١.٥ dB Q \U0001d7d0",
    "Preamp: १२.५ dB", "Filter 1: ON PK Fc ١.٢.٣ Hz Gain 1 dB",
]

VALUES = [0.0, -0.0, 1.0, -1.0, 0.5, 1.5, 2.5, 0.25, 0.125, 2 ** -12, 1e5, 1e6, 123456.5, 999999.5, 9999995.0, 99999995.0,
          999999995.0, 0.0001, 0.00001, 0.000099999999, 123456789.0, 1e-5, 1e16, 1e22, 5e-324, 1.7976931348623157e308,
          0.70710678, 0.35355339, 0.8660, 2.26464, 125.0, 23000.0, 12345.67, 105.0, -2.6, 0.707, 1 / 3, 2 / 3, -11.0, 0.1,
          10 ** (-11.0 / 20), 10 ** (-9.4 / 20), 10 ** (-5.7 / 20), 10 ** (-8.3 / 20), 10 ** (20 / 20), 10 ** (-60 / 20),
          float("inf"), float("-inf")]


def r(value: float) -> str:
    return repr(float(value))


def control_list(values: dict) -> list:
    return [[name, r(value)] for name, value in values.items()]


def argument(values: dict) -> str:
    """What the tool passes to `pw-cli set-param <id> Props` for these controls."""
    return "{ params = [ " + " ".join(f'"{k}" {tool.node_value(v)}' for k, v in values.items()) + " ] }"


def shown(params: dict) -> str:
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        tool.show(params)
    return out.getvalue()


def parsed(text: str) -> dict:
    try:
        preamp, filters = eq.parse(text)
    except eq.EqError as error:
        return {"text": text, "error": str(error), "line": error.line}
    return {"text": text, "preamp": r(preamp),
            "filters": [{"type": f["type"], "freq": r(f["freq"]), "gain": r(f["gain"]), "q": r(f["q"])} for f in filters]}


def preset_of(text: str) -> dict:
    preamp, filters = eq.parse(text)
    return {"preamp": preamp, "filters": filters}


def build() -> dict:
    reference: dict = {}

    # --- the tool's tables
    reference["tables"] = {
        "bw4_q": [r(q) for q in tool.BW4_Q], "groups": tool.GROUPS, "positions": tool.POSITIONS, "left": tool.LEFT,
        "right": tool.RIGHT, "eq_slots": [list(slot) for slot in tool.EQ_SLOTS], "node": tool.NODE,
        "loudness_offset": {key: r(value) for key, value in tool.LOUDNESS_OFFSET.items()},
        "default_state": {"preset": tool.DEFAULT_STATE["preset"], "preamp": r(tool.DEFAULT_STATE["preamp"]),
                          "match": tool.DEFAULT_STATE["match"]},
        "preset_keys": list(tool.PRESETS), "node_id": NODE_ID,
    }
    reference["constants"] = {
        "tool": {"NODE_ID_TTL": r(tool.NODE_ID_TTL)},
        "downmix": {"APPLY_TIMEOUT_S": r(downmix.APPLY_TIMEOUT_S), "ORDER": list(downmix.ORDER), "LABELS": downmix.LABELS,
                    "DESCRIPTIONS": downmix.DESCRIPTIONS},
        "eq": {"SAMPLE_RATE": eq.SAMPLE_RATE, "EQ_SLOTS": [[kind, slots] for kind, slots in eq.EQ_SLOTS.items()],
               "FILTER_TYPES": list(eq.FILTER_TYPES), "NAME_MAX": eq.NAME_MAX, "CURVE_POINTS": eq.CURVE_POINTS,
               "STATE_VERSION": eq.STATE_VERSION,
               "ranges": {name: [r(low), r(high)] for name, (low, high) in (
                   ("PREAMP_RANGE", eq.PREAMP_RANGE), ("FREQ_RANGE", eq.FREQ_RANGE), ("GAIN_RANGE", eq.GAIN_RANGE),
                   ("Q_RANGE", eq.Q_RANGE), ("CURVE_RANGE", eq.CURVE_RANGE))}},
    }
    presets = {}
    for key in tool.PRESETS:
        params = tool.resolve(argparse.Namespace(preset=key))
        values = tool.controls(params)
        presets[key] = {"params": {name: r(value) for name, value in params.items()}, "controls": control_list(values),
                        "argument": argument(values), "show": shown(params)}
    reference["presets"] = presets

    # --- amui/downmix.py
    reference["profiles"] = downmix.profiles()
    reference["expected_controls"] = [
        {"preset": key, "preamp": r(preamp), "controls": control_list(downmix.expected_controls(key, preamp))}
        for key in tool.PRESETS for preamp in (0.0, -11.0, -9.3333333, 20.0)]
    applies = []
    for key in tool.PRESETS:
        for preamp in (-11.0, -9.0, -9.3333333, -0.00004, 20.0):
            for match in (True, False):
                final = downmix.effective_preamp(key, preamp, match)
                # the engine ran `downmix.py apply <key> --preamp {final:.4f}`; the tool's main() resolves that to:
                params = tool.resolve(argparse.Namespace(preset=key, preamp=float(f"{final:.4f}")))
                applies.append({
                    "key": key, "preamp": r(preamp), "match": match, "final": r(final),
                    "argument": argument(tool.controls(params)),
                    "output": f"applied to node {NODE_ID} ({tool.NODE})\n" + shown(params),
                    "audit": f"applied preset={key} preamp={preamp:+.2f} final={final:+.2f} match={match}",
                })
    reference["apply"] = applies

    states = []
    for text in ['{"preset": "itu", "preamp": -7.5, "match": false}', '{"preset": "apple"}', '{"preamp": -3, "other": 1}',
                 '{"match": 0}', "{}", "not json", None]:
        path = SCRATCH / "downmix-state.json"
        if text is None:
            path.unlink(missing_ok=True)
        else:
            path.write_text(text)
        state = tool.saved_state(str(path))
        states.append({"file": text, "preset": state["preset"], "preamp": r(state["preamp"]), "match": bool(state["match"])})
    reference["saved_state"] = states

    # --- number formats
    reference["formats"] = [{"value": r(v), "node_value": tool.node_value(v), "g": f"{v:g}"} for v in VALUES]

    # --- EQ text
    reference["texts"] = {"apo": APO_TEXT, "peqdb": PEQDB_TEXT, "no_q": NO_Q_TEXT, "edges": EDGES_TEXT}
    reference["parse"] = [parsed(text) for text in PARSE_TEXTS]
    eq_cases = []
    for name, text, preamp in (("apo", APO_TEXT, -6.2), ("peqdb", PEQDB_TEXT, -10.4), ("flat", "", 0.0), ("trim", "", -8.3)):
        values = tool.eq_controls(preamp, eq.parse(text)[1])
        eq_cases.append({"name": name, "text": text, "preamp": r(preamp), "controls": control_list(values),
                         "argument": argument(values)})
    reference["eq_controls"] = eq_cases
    reference["preset_text"] = [
        {"text": text, "written": eq.preset_text(preset_of(text))}
        for text in (APO_TEXT, PEQDB_TEXT, NO_Q_TEXT, "", "Preamp: -0 dB",
                     "Preamp: 3.25 dB\nFilter 1: ON LS Fc 12345.67 Hz Gain -0.05 dB Q 0.123456789")]
    names = ["  My IEMs  ", "Reference", "Ünïcödé Nämé", "İstanbul", "ΣΑΣ",
             "a" * 50 + "-b", "a" * 39 + "--b", "--x--", "", "日本語", "A_b.C", "ß", "Über 9000!", "   ",
             "x" * 100]
    reference["slug"] = [[name, eq._slug(name)] for name in names]

    # --- response curves
    freqs = [10.0, 20.0, 55.5, 100.0, 440.0, 1000.0, 3200.0, 10000.0, 20000.0, 23999.0, 24000.0]
    curves = []
    for name, text in (("apo", APO_TEXT), ("peqdb", PEQDB_TEXT), ("no_q", NO_Q_TEXT), ("flat", ""),
                       ("minus zero", "Preamp: -0 dB"), ("edges", EDGES_TEXT)):
        preset = preset_of(text)
        case = {"name": name, "text": text, "freqs": [r(f) for f in freqs],
                "response": [r(g) for g in eq.response(preset, freqs)],
                "curve5": [[r(f), r(g)] for f, g in eq.curve(preset, 5)],
                "spark": [r(g) for g in eq.response(preset, eq.default_freqs(64))]}
        if name in ("peqdb", "edges"):
            case["curve"] = [[r(f), r(g)] for f, g in eq.curve(preset)]
        curves.append(case)
    reference["curves"] = curves
    reference["default_freqs"] = {str(n): [r(f) for f in eq.default_freqs(n)] for n in (0, 1, 2, 5, 64, 200)}
    reference["biquad"] = [
        {"kind": kind, "freq": r(freq), "gain": r(gain), "q": r(q),
         "coefficients": [r(c) for c in eq.biquad_coefficients(kind, freq, gain, q)]}
        for kind, freq, gain, q in (("PK", 1000.0, 6.0, 1.0), ("PK", 105.0, -2.6, 0.7), ("LSC", 60.0, 3.5, 0.71),
                                    ("HSC", 10000.0, 3.0, 0.7), ("LSC", 10.0, -30.0, 0.05), ("HSC", 23000.0, 30.0, 20.0),
                                    ("PK", 23000.0, 0.0, 0.707))]

    # --- state files: what _clean_preset keeps, what the Equalizer writes
    raw_presets = [
        {"id": "ok-1", "name": "Ok", "preamp": 0, "filters": []},
        {"id": "full-2", "name": "  Full  ", "preamp": -5.7, "source": "sample", "text": "  Preamp: -5.7 dB \n", "added_at": 12,
         "filters": [{"type": "pk", "freq": 1000, "gain": -2.0, "q": 1.0}, {"type": "LSC", "freq": "100", "gain": 2}]},
        {"id": "strings-3", "name": 7, "preamp": "3", "filters": [], "added_at": "soon", "text": "   ", "source": 0},
        {"id": "bools-4", "name": True, "preamp": True, "filters": [], "added_at": True},
        {"id": "null-preamp", "preamp": None, "filters": []},
        {"id": "no-preamp", "filters": []},
        {"id": "no-filters"},
        {"id": "null-filters", "filters": None},
        {"id": "loud", "preamp": 20.5, "filters": []},
        {"id": "quiet", "preamp": -40, "filters": []},
        {"id": "nan", "preamp": "nan", "filters": []},
        {"id": "words", "preamp": "minus three", "filters": []},
        {"id": "   ", "filters": []},
        {"id": "x" * 120, "filters": []},
        {"id": "x" * 121, "filters": []},
        {"id": 5, "filters": []},
        {"name": "No id", "filters": []},
        {"id": "bad-type", "filters": [{"type": "XX", "freq": 100, "gain": 1}]},
        {"id": "no-type", "filters": [{"freq": 100, "gain": 1}]},
        {"id": "no-freq", "filters": [{"type": "PK", "gain": 1}]},
        {"id": "null-q", "filters": [{"type": "PK", "freq": 100, "gain": 1, "q": None}]},
        {"id": "low-freq", "filters": [{"type": "PK", "freq": 9, "gain": 1}]},
        {"id": "high-gain", "filters": [{"type": "PK", "freq": 100, "gain": 31}]},
        {"id": "low-q", "filters": [{"type": "PK", "freq": 100, "gain": 1, "q": 0.01}]},
        {"id": "not-a-list", "filters": {"type": "PK"}},
        {"id": "not-a-filter", "filters": ["PK"]},
        {"id": "five-shelves", "filters": [{"type": "HSC", "freq": 9000, "gain": 1}] * 5},
        {"id": "four-shelves", "filters": [{"type": "hsc", "freq": 9000, "gain": 1}] * 4},
        {"id": "long-name", "name": "n" * 100, "filters": []},
        {"id": "empty-name", "name": "", "filters": []},
        {"id": "spaced-name", "name": " \t ", "source": "bridge", "filters": []},
        "not a preset", None, 3,
    ]
    reference["clean_preset"] = [
        {"raw": raw, "state": eq._clean_preset(raw), "bridge": eq._clean_preset(raw, source="bridge")} for raw in raw_presets]

    # A state file as the Python Equalizer writes it, from a bridge file with two usable presets.
    bridge = SCRATCH / "bridge-eq.json"
    bridge.write_text(json.dumps({"enabled": True, "active": "beta-0002", "volume": 100, "presets": [
        {"id": "alpha-0001", "name": "Alpha", "preamp": -5.7, "text": "Preamp: -5.7 dB", "added_at": 1700000000.5,
         "filters": [{"type": "PK", "freq": 1000, "gain": -2.0, "q": 1.0}, {"type": "LSC", "freq": 100, "gain": 2.0, "q": 0.7}]},
        {"id": "beta-0002", "name": "Beta", "preamp": -3.0, "text": "Preamp: -3.0 dB", "added_at": 1700000001,
         "filters": [{"type": "HSC", "freq": 9000, "gain": -1.5, "q": 0.7}]},
        {"id": "broken-0003", "name": "Broken", "preamp": -3.0, "filters": [{"type": "XX", "freq": 1}]},
    ]}, indent=1))
    state_path = SCRATCH / "state" / "eq.json"
    equalizer = eq.Equalizer(path=state_path, bridge_path=bridge, apply_fn=lambda values: None, live_fn=lambda: {},
                             audit=lambda message: None)
    equalizer.set_enabled(True)
    equalizer.move_preset("beta-0002", 0)
    written = json.loads(state_path.read_text())
    written["updated_at"] = 1700000002.25  # the one value that is the time of writing
    cache._write_private_json(state_path, written)
    reference["state_file"] = {"bridge": bridge.read_text(), "text": state_path.read_text(),
                               "snapshot": equalizer.snapshot(), "controls": control_list(equalizer.controls()),
                               "controls_trim": control_list(equalizer.controls(-2.0))}

    # A downmix state file as amui/downmix.py writes it after an apply.
    downmix_path = SCRATCH / "state" / "downmix.json"
    cache._write_private_json(downmix_path, {"preset": "apple", "preamp": -9.0, "match": True, "applied_at": 1700000003.5})
    reference["downmix_state_file"] = downmix_path.read_text()
    return reference


def write(reference: dict, out) -> None:
    """One line per case: sections as objects or lists whose members are written compactly."""
    dumps = lambda value: json.dumps(value, ensure_ascii=True)  # noqa: E731
    out.write("{\n")
    for index, (name, section) in enumerate(reference.items()):
        if isinstance(section, list):
            body = "[\n" + ",\n".join("  " + dumps(item) for item in section) + "\n ]"
        elif isinstance(section, dict):
            body = "{\n" + ",\n".join(f"  {dumps(key)}: {dumps(value)}" for key, value in section.items()) + "\n }"
        else:
            body = dumps(section)
        out.write(f" {dumps(name)}: {body}" + ("," if index + 1 < len(reference) else "") + "\n")
    out.write("}\n")


if __name__ == "__main__":
    try:
        write(build(), sys.stdout)
    finally:
        shutil.rmtree(SCRATCH, ignore_errors=True)
