# Equalizer (`amui/eq.py`, `app/eq_backend.py`, `app/qml/Eq*.qml`)

Headphone EQ presets "like the other clone": an on/off switch, the active preset's frequency
response, reorderable preset cards, a sheet to paste EqualizerAPO / AutoEQ / PEQdB text, import of
the old clone's presets. The EQ itself runs in the **shared PipeWire sink** "Atmos 9.1.6 Downmix"
(the workspace's filter chain): after the matrix and the preamp come, per side, a gain stage
`eqgain_{l,r}:Mult` and fixed biquad slots `eq_{l,r}_lsc1..4` (low shelf), `eq_{l,r}_pk1..24`
(peaking), `eq_{l,r}_hsc1..4` (high shelf) with controls `:Freq`, `:Q`, `:Gain`. Nothing here is
applied when the app starts or a module loads: the sink changes only when the user acts, every change
is appended to `var/logs/downmix.log` (`amui.downmix._audit`, lines starting `eq applied` /
`eq FAILED`), and the matrix / preamp controls are never touched (the engine only emits `eq*` keys).

## Files

| file | what |
|---|---|
| `amui/eq.py` | the engine: parse (the workspace's `parse_eq`), RBJ response curves, the state file, bridge import, `apply()` through the workspace's `eq_controls` + `apply_controls`, `live_state()` from `downmix.live_controls()` |
| `app/eq_backend.py` | `EqBackend(QObject)`: the QML contract (`EqApp`), a one-thread `QThreadPool` for every engine call, the 5 s live refresh, fake mode; `install(engine, backend)` |
| `app/qml/EqualizerPage.qml` | the page (header row + switch, graph card, live note, preset cards, buttons, footer) |
| `app/qml/EqGraph.qml` | Canvas graph: log 20 Hz-20 kHz axis with labels, ±12 dB axis that grows in 6 dB steps to fit, dashed 0 dB line, curve filled towards 0 dB in `Theme.accent` (grey when off) |
| `app/qml/EqPresetCard.qml` | a card: sparkline (64 points), name, "15 filters · Preamp -10.4 dB", source; accent outline when active; "…" / right click -> menu |
| `app/qml/EqMenu.qml`, `EqSheet.qml` | the card menu (Rename, Move Up/Down, Delete) and the modal sheet (paste with live preview, rename, delete confirmation) |
| `tests/test_eq.py` | 16 tests: engine (stubbed sink, temp state) + the page on the fake backend, offscreen, zero QML warnings |
| `app/screenshots/equalizer.png` | headless GPU grab of the page with the fake presets (regenerate: see below) |

## State

`var/state/eq.json` (0600, atomic replace through `amui.cache._write_private_json`):

```json
{"version": 1, "enabled": false, "active": "<id>|null", "bridge_imported": true, "updated_at": 0.0,
 "presets": [{"id", "name", "preamp", "filters": [{"type": "PK|LSC|HSC", "freq", "gain", "q"}],
              "source": "pasted|bridge|sample", "text": "<the pasted text>", "added_at": 0.0}],
 "order": ["<id>", "..."]}
```

Same preset shape as the bridge's `~/.config/applemusic-bridge/eq.json`, which is **read only**:
on first use (no state file yet) every bridge preset that parses is copied (`source: "bridge"`,
by id) and the bridge's `active` id carries over with `enabled` **false** (the sink's current state
is never changed by loading); "Import from the old clone" (`import_bridge_presets()`) copies the
ones not present yet, at any time. Presets read from either file are validated like `parse_eq`
(ranges, slot counts); damaged entries are skipped, duplicates by id dropped, `order` made
consistent with `presets`. A corrupt state file starts empty without a second import.

## Engine API (`amui.eq`)

Module-level functions delegate to one process-wide `Equalizer` (created on first use, on
`config.STATE_DIR/eq.json`); tests and the fake UI build their own `Equalizer(path, bridge_path,
apply_fn, live_fn, audit)` (`path=None` = in memory; `apply_fn` / `live_fn` stand in for the sink,
then nothing is audit-logged unless `audit` is given). All calls take one re-entrant lock.

* `parse(text) -> (preamp_db, filters)`; `EqError(message, line)` with `line` = the offending line of
  pasted text (`"line 3: not a Preamp or Filter line: …"`) or `None` for totals (preamp range, too
  many filters of a kind). `OFF` filters and `#` comments are skipped; `PEQ`/`LS`/`HS` aliases and a
  missing Q (0.707) are accepted, exactly as the workspace's `parse_eq` (used when the workspace is
  there; `eq._Fallback` is a copy of its pure functions for machines without it, tested equal).
* `presets()`, `preset(id)`, `active()`, `enabled()`, `snapshot()`, `reload()`.
* `set_enabled(bool)`, `set_active(id|None)`, `add_preset(name, text, source="pasted") -> preset`,
  `rename_preset(id, name)`, `delete_preset(id)` (the active one clears `active`),
  `move_preset(id, new_index) -> [ids]` (clamped), `import_bridge_presets() -> int`.
* `response(preset, freqs) -> [dB]` (RBJ cookbook biquads at 48 kHz, the preamp included),
  `curve(preset, n=200) -> [[Hz, dB]]` (log-spaced 20 Hz-20 kHz), `biquad_coefficients(...)`,
  `default_freqs(n)`, `preset_text(preset)` (EqualizerAPO text again).
* `controls(trim_db=0.0) -> {control: value}`: the active preset when enabled
  (`eq_controls(preamp + trim_db, filters)`), else flat (`eq_controls(trim_db)`); only `eq*` keys.
* `apply(trim_db=0.0) -> controls`: `controls()` set on the running sink with the workspace's
  `apply_controls(c, config.SINK_NAME)` after `downmix.sink_node_id()` confirmed it runs
  (`EqError` otherwise, logged `eq FAILED`). `trim_db` is reserved for a Sound Check trim: the
  playback side applies loudness in software, so the UI always passes 0.
* `live_state() -> {"enabled_in_sink": bool, "bands": n, "gain_db": float}` from
  `downmix.live_controls()` (`bands` = non-zero `eq_l_*:Gain` slots, `gain_db` from `eqgain_l:Mult`).

## QML contract (`EqApp`)

Properties (all notifiable): `enabled`, `activeId`, `activeName`, `activeMeta` ("15 filters ·
Preamp -10.4 dB"), `presets` (`[{id, name, preamp, filterCount, source, sourceLabel, meta, spark}]`,
`spark` = 64 dB values for the card), `curve` (`[[Hz, dB]] × 200` of the active preset, flat when
none), `liveNote` ("Sink: EQ on, 15 bands, -10.4 dB", refreshed every 5 s and after every change;
with " · flat now: another app changed it, toggle to apply again" or " · set by another app" when
the sink disagrees with this app's state; "Sink: downmix sink is not running" when it is gone),
`error` (last failure, cleared by the next success), `busy`, `previewError` / `previewInfo` (the
sheet's parse result), `fake`, `bridgeAvailable` (the bridge's file exists), `ready`.

Invokables: `setEnabled(bool)`, `activate(id)` (also turns the EQ on, like clicking a card in the
other clone), `addFromText(name, text) -> bool` (false + `previewError` when the text does not
parse; true = parsed and being stored), `rename(id, name)`, `remove(id)` (the active one of an
enabled EQ sets the sink flat), `move(id, index)`, `importFromBridge()` (toasts the count),
`previewCurve(text) -> [[Hz, dB]]` (sets `previewError` / `previewInfo`), `presetText(id)`,
`refresh()` (re-read the file and the sink, applies nothing), `refreshLive()`, `clearError()`,
`shutdown()`; signal `changed()` after every state update. Errors also go to the App backend's
`toast` signal. Threading: every engine call runs on a private one-thread pool (user actions keep
their order), results are marshalled with a queued signal; only the GUI thread touches properties.

### `install(engine, backend)` and the Qt 6.11 module quirk

`main.py` calls `app.eq_backend.install(engine, backend)` right after creating the engine and
before `engine.load(Main.qml)`. With an engine given, `EqApp` is set as a **root-context property**,
not registered into module `AmUi`: in this PySide6 6.11.2 build, registering a type or a singleton
*instance* into `AmUi` after a `QQmlApplicationEngine` exists breaks the resolution of the module's
sibling QML files (`Spinner.qml: Cannot assign object of type "QQuickRepeater" to list property
"data"`), the quirk main.py already notes for composite singletons; a context property never touches
the type registry. Even a `qmlTypeId("AmUi", 1, 0, "App")` probe before the registrations triggers
it (seen while writing the test). Called with `engine=None` *before* the engine is created, `install`
registers the proper singleton `AmUi.EqApp` 1.0 instead. QML reads `EqApp.enabled` the same way in
both cases; `install` returns the object, also reachable as `backend.eq`, and a second call returns
the first object. Fake mode (`backend.fake`): an in-memory `Equalizer` with three sample presets
("Reference", "Warm", "Bright") and a stand-in sink that remembers what was applied, so the live note
reacts; PipeWire is never touched.

What the UI side still has to add (not in these files): the sidebar entry, `pageTitles.equalizer`,
`PageSlot { pageId: "equalizer"; sourceComponent: EqualizerPage {} }` in `Main.qml`, `"equalizer"`
in `main.py`'s `PAGES`, and the `install` call.

## Verification

* `python3 -m unittest tests.test_eq -v`: 16 tests, ~2 s, all passing. Engine: APO / AutoEQ / PEQdB
  texts (preamp, `OFF`, comments, aliases, default Q), errors with line numbers, slot overflow (25 PK,
  5 LSC), the fallback parser equal to the workspace's, response sanity (+6 dB PK at 1 kHz Q 1:
  6.000 dB at 1 kHz, |0.07| dB at 100 Hz, |0.05| dB at 10 kHz; shelves; preamp), state persistence
  (0600, order, rename/move/delete, damaged files), bridge import (first use, by id, invalid skipped,
  file untouched), `apply` through a stub (slot layout, trim on the gain stage only, flat), `apply`
  without the sink (`EqError`, `eq FAILED` logged, `apply_controls` never called). Page: loads with
  the fake presets, switch / activate / move / remove / import from QML expressions, the paste sheet
  (parse error on line 4 blocks Add, a valid text previews "1 filter · Preamp -6.0 dB" and adds a
  card), the card menu, deleting the active preset goes flat; zero QML warnings throughout.
* Screenshot: `AMUI_TEST_PLATFORM=headless AMUI_EQ_GRAB=app/screenshots/equalizer.png python3 -m
  unittest tests.test_eq.EqPageTest` (GPU render node, OpenGL, 0 warnings) - the page in a test
  window of the content area's size with a header strip, EQ on, "Reference" active.
* **Live round trip** (2026-11-08 02:43, one apply and one restore, ~1 s apart, node 38):
  found `eqgain_l/r:Mult = 0.384592` (−8.30 dB, the other clone's Sound Check trim) and 0 non-zero
  bands; `apply()` of the imported EM6L preset (15 filters, preamp −10.4 dB) in 0.11 s: read back 15
  non-zero bands and `eqgain = 0.301995` (−10.400 dB), none of the 194 controls differing from what
  was sent beyond float32; then `set_enabled(False)` + `apply(trim_db=−8.29999508)` (the gain found):
  0 bands, `eqgain = 0.384592` on both sides, **no EQ and no non-EQ control differing from the state
  found**; audit log has both lines. am-ui's state is now `enabled: false`, `active: <the bridge's
  active id>` (16 presets imported on first use). Caveat: at that moment `wpctl` showed the default
  output (the USB DAC) at 0.88 and *not* muted (only the internal analog sink was); the downmix sink
  itself was at 1.00. The premise "both sinks muted" did not hold, so if music was playing through
  the downmix for that second it was EQ'd and 2.1 dB quieter.

## Open issues / what to know

* **One gain stage, two apps.** `eqgain` carries the other clone's preset preamp + Sound Check trim +
  volume. When this app applies a preset it sets `eqgain = preamp + 0`, dropping that trim until the
  bridge sets it again (every song: its `_apply_gain` writes `eqgain` alone, so after that *our*
  biquads run with *its* gain stage: an EQ with +6 dB boosts and a preamp of 0 can clip). And its
  `watch_node` re-applies its whole EQ (flat, when its EQ is off) whenever the downmix node starts
  again, which removes ours; the live note then says "flat now: another app changed it, toggle to
  apply again". Nothing is written into the PipeWire config (by design), so a restart always comes
  back with the bridge's EQ. A follow-up could compute the bridge's share of the gain stage from
  its eq.json (`volume_db(volume)` + its preset's preamp when enabled) and keep the remainder (the
  trim) as `trim_db` - that is what the restore step of the live check did by hand.
* The shelves are drawn with the RBJ Q-based formulas (as the old clone's graph); PipeWire's
  `bq_lowshelf` / `bq_highshelf` were not checked against them, so the drawn shelf slopes may differ
  slightly from what plays. Peaking filters are exact.
* No drag-and-drop reordering of the cards (the menu's Move Up / Move Down instead, like the queue);
  no "Edit filters" of an existing preset (paste a new one; `presetText(id)` returns the text).
* `previewCurve` parses on the GUI thread (regexes + 200 points, well under a millisecond per
  filter); `addFromText` parses there too, then stores on the pool.
* The Canvas labels use the application font's *resolved* family (a hidden `Text`'s `fontInfo`):
  `Theme.fontFamily` names the first of the fallback list ("SF Pro Text"), which Context2D rejects.
