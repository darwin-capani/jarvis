#!/usr/bin/env python3.11
"""Stdlib-only unit tests for the Nexus control plane (apps/nexus/main.py).

Scope (the python-control module's verifiable surface — SPEC §5/§6):
  - op dispatch: route.set / gain.set / mute / monitor.set / state.get onto the
    real native engine, with the ack/telemetry side-effects captured,
  - gain/index clamping (out-of-range rejected, never crashes the dispatcher),
  - preset TOML save+load IDENTITY through the module's own serializer/parser,
  - telemetry payload SHAPES match the SPEC §6 topics (levels/spectrum/routes/
    gain), and ride the daemon-relayable `type:"items"` wire shape,
  - the capability TOKEN is stamped on EVERY emitted line,
  - preset-name PATH CONFINEMENT (no traversal out of presets/ or scratch/).

NO socket, NO device, NO audio: a FakeLink captures outbound lines in memory,
and the engine is driven purely through the FFI on synthesized state. Tests that
need the native core are skipped (not failed) if the cdylib is not yet built, so
this file is runnable headlessly even before a `cargo build`; the pure-Python
tests (TOML round-trip, path confinement, wire shape, token discipline) always
run.

Run: python3.11 -m unittest apps/nexus/test_main.py  (or `-m unittest` from here)
"""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

import main as nexus


# --------------------------------------------------------------------------- #
# Test doubles.
# --------------------------------------------------------------------------- #
class FakeLink:
    """A HostLink stand-in that captures every outbound line in memory instead
    of writing to a socket. Mirrors HostLink's public surface (send / telemetry
    / log) AND its token-stamping + wire framing, so token-discipline and
    payload-shape assertions test the real serialization path."""

    def __init__(self, token: str = "tok-CAFEBABE") -> None:
        self._token = token
        self.lines: list[dict] = []          # decoded {token,type,data} dicts
        self.raw: list[str] = []             # the exact JSON strings written

    def send(self, msg_type: str, data: dict) -> None:
        # Identical framing to HostLink.send (token stamped, compact separators).
        line = json.dumps(
            {"token": self._token, "type": msg_type, "data": data},
            ensure_ascii=False,
            separators=(",", ":"),
        )
        self.raw.append(line)
        self.lines.append(json.loads(line))

    def telemetry(self, topic: str, payload: dict) -> None:
        # Same shape HostLink.telemetry produces: type "items", with the payload
        # fields FLATTENED into data alongside topic ({**payload, "topic": topic}),
        # matching Vision + the HUD parsers (no nested "payload" wrapper). topic is
        # applied last so a stray payload "topic" key can't override routing.
        self.send("items", {**payload, "topic": topic})

    def log(self, line: str) -> None:
        self.send("log", {"line": line})

    # -- assertion helpers --
    def telemetry_for(self, topic: str) -> list[dict]:
        out = []
        for ln in self.lines:
            if ln["type"] == "items" and ln["data"].get("topic") == topic:
                # Flattened wire: payload fields sit in data alongside topic. Return
                # just the fields (topic stripped) so callers assert on the payload.
                out.append({k: v for k, v in ln["data"].items() if k != "topic"})
        return out

    def logs(self) -> list[str]:
        return [ln["data"]["line"] for ln in self.lines if ln["type"] == "log"]


def _load_core_or_skip() -> "nexus.NexusCore":
    """Load the real native core, or skip the test if the cdylib isn't built."""
    try:
        return nexus.load_core()
    except nexus.NexusCoreError as exc:  # cdylib not built yet
        raise unittest.SkipTest(f"nexus_core cdylib unavailable: {exc}")


# --------------------------------------------------------------------------- #
# Pure-Python: TOML round-trip + preset-name confinement (no native core).
# --------------------------------------------------------------------------- #
class TestPresetToml(unittest.TestCase):
    def test_route_toml_roundtrip_identity(self):
        # The module's _to_toml emitter and tomllib parser must round-trip a
        # [[route]] preset with no drift in in/out/gain_db.
        doc = {
            "route": [
                {"in": 0, "out": 0, "gain_db": 0.0},
                {"in": 1, "out": 2, "gain_db": -3.5},
                {"in": 3, "out": 1, "gain_db": 12.0},
            ]
        }
        text = nexus._to_toml(doc)
        with tempfile.TemporaryDirectory() as td:
            p = Path(td) / "vocal.toml"
            p.write_text(text, encoding="utf-8")
            back = nexus._read_toml(p)
        self.assertEqual(back["route"], doc["route"])

    def test_toml_roundtrip_preserves_float_and_int_types(self):
        doc = {"route": [{"in": 2, "out": 3, "gain_db": -6.0}]}
        back = nexus._read_toml_from_str(nexus._to_toml(doc))
        r = back["route"][0]
        self.assertIsInstance(r["in"], int)
        self.assertIsInstance(r["out"], int)
        self.assertIsInstance(r["gain_db"], float)
        self.assertEqual(r["gain_db"], -6.0)

    def test_safe_preset_name_accepts_simple_slug(self):
        # The strict allowlist is [A-Za-z0-9._-]; an interior dot is fine.
        for ok in ("vocal", "podcast_a", "Take-3", "mix.v2", "A1_b-2.x"):
            self.assertEqual(nexus._safe_preset_name(ok), ok)

    def test_safe_preset_name_rejects_traversal(self):
        for bad in ("../etc/passwd", "a/b", "..", ".hidden", "x\\y", "", "  "):
            with self.assertRaises(ValueError):
                nexus._safe_preset_name(bad)

    def test_safe_preset_name_rejects_special_chars(self):
        # The tightened allowlist rejects anything outside [A-Za-z0-9._-]:
        # spaces, '=', quotes, newline, NUL, '*', and other glob/shell/TOML
        # metacharacters that the old blocklist let through.
        for bad in (
            "my preset",   # space
            "a=b",          # equals
            'q"x',          # double quote
            "q'x",          # single quote
            "line\nbreak",  # newline
            "nul\x00byte",  # NUL
            "glob*",        # asterisk
            "semi;colon",   # semicolon
            "back`tick",    # backtick
            "dollar$ign",   # dollar
            "perc%ent",     # percent
            "café",         # non-ASCII
        ):
            with self.assertRaises(ValueError):
                nexus._safe_preset_name(bad)

    def test_safe_preset_name_strips_whitespace(self):
        self.assertEqual(nexus._safe_preset_name("  vocal  "), "vocal")


# --------------------------------------------------------------------------- #
# Pure-Python: the telemetry wire shape the daemon will actually relay.
# --------------------------------------------------------------------------- #
class TestTelemetryWireShape(unittest.TestCase):
    def test_telemetry_uses_items_type_not_telemetry(self):
        # daemon/src/apps.rs classify_inbound_line drops any type other than
        # items/status/log; a telemetry drop MUST be "items".
        link = FakeLink()
        link.telemetry("audio.levels", {"ch": []})
        self.assertEqual(link.lines[0]["type"], "items")

    def test_telemetry_flattens_payload_fields_into_data(self):
        # FLAT wire: payload fields sit DIRECTLY in data alongside topic (like Vision
        # + the HUD parsers), NOT under a nested "payload" object — else every HUD
        # field is one level too deep and the panel renders blank.
        link = FakeLink()
        link.telemetry("audio.spectrum", {"bands": [0.0] * 96})
        data = link.lines[0]["data"]
        self.assertEqual(data["topic"], "audio.spectrum")
        self.assertNotIn("payload", data, "telemetry must be flat (no nested payload wrapper)")
        self.assertEqual(data["bands"], [0.0] * 96)

    def test_clipping_payload_is_flat_channel_and_true_peak(self):
        # audio.clipping rides the same FLAT wire as the other nexus topics:
        # {channel, true_peak_dbfs} sit DIRECTLY in data alongside topic (what the
        # HUD's parseNexusClipping reads), NOT under a nested payload wrapper.
        link = FakeLink()
        link.telemetry("audio.clipping", {"channel": 2, "true_peak_dbfs": -0.3})
        data = link.lines[0]["data"]
        self.assertEqual(data["topic"], "audio.clipping")
        self.assertNotIn("payload", data)
        self.assertEqual(data["channel"], 2)
        self.assertEqual(data["true_peak_dbfs"], -0.3)

    def test_every_emitted_line_carries_the_token(self):
        link = FakeLink(token="cap-123")
        link.telemetry("audio.routes", {"revision": 1})
        link.log("hello")
        link.send("status", {"ok": True})
        self.assertTrue(link.lines)
        for ln in link.lines:
            self.assertEqual(ln["token"], "cap-123")

    def test_emitted_lines_are_single_line_json(self):
        link = FakeLink()
        link.telemetry("audio.levels", {"ch": [{"peak_dbfs": -6.0, "rms_dbfs": -9.0}]})
        for raw in link.raw:
            self.assertNotIn("\n", raw)
            json.loads(raw)  # must parse


# --------------------------------------------------------------------------- #
# Pure-Python: the -inf/NaN -> None JSON-safety fold.
# --------------------------------------------------------------------------- #
class TestFiniteFold(unittest.TestCase):
    def test_finite_maps_non_finite_to_none(self):
        self.assertIsNone(nexus._finite(float("-inf")))
        self.assertIsNone(nexus._finite(float("inf")))
        self.assertIsNone(nexus._finite(float("nan")))
        self.assertIsNone(nexus._finite(None))

    def test_finite_passes_through_real_values(self):
        self.assertEqual(nexus._finite(-6.0), -6.0)
        self.assertEqual(nexus._finite(0.0), 0.0)

    def test_levels_payload_is_json_serializable_for_silence(self):
        # Silence reads -inf from the engine; the fold must make the payload
        # encodable (JSON has no -inf).
        payload = {
            "ch": [{"peak_dbfs": nexus._finite(float("-inf")),
                    "rms_dbfs": nexus._finite(float("-inf"))}],
            "lufs_m": nexus._finite(float("-inf")),
        }
        json.dumps(payload)  # must not raise


# --------------------------------------------------------------------------- #
# Native-core: op dispatch onto the real engine (skipped if cdylib not built).
# --------------------------------------------------------------------------- #
class TestOpDispatch(unittest.TestCase):
    def setUp(self):
        self.core = _load_core_or_skip()
        self.link = FakeLink()
        self.disp = nexus.OpDispatcher(self.core, self.link)
        self.addCleanup(self.core.close)

    def test_route_set_applies_crosspoint_and_emits_routes(self):
        self.disp.dispatch("route.set", {"op": "route.set", "in": 0, "out": 1, "gain_db": -6.0})
        self.assertAlmostEqual(self.core.crosspoint(0, 1), -6.0, places=4)
        routes = self.link.telemetry_for("audio.routes")
        self.assertTrue(routes, "route.set must emit an audio.routes telemetry")
        # Crosspoints ride the WIRE under "matrix" (what the HUD's
        # parseNexusRoutes reads); "route" is only the preset-TOML table name.
        self.assertIn({"in": 0, "out": 1, "gain_db": -6.0}, routes[-1]["matrix"])
        self.assertNotIn("route", routes[-1])

    def test_route_set_clears_with_minus_inf(self):
        self.disp.dispatch("route.set", {"in": 0, "out": 0, "gain_db": -3.0})
        self.assertAlmostEqual(self.core.crosspoint(0, 0), -3.0, places=4)
        self.disp.dispatch("route.set", {"in": 0, "out": 0, "gain_db": "off"})
        self.assertEqual(self.core.crosspoint(0, 0), float("-inf"))

    def test_route_set_missing_gain_clears(self):
        self.disp.dispatch("route.set", {"in": 1, "out": 1, "gain_db": 0.0})
        self.disp.dispatch("route.set", {"in": 1, "out": 1})  # no gain_db -> -inf
        self.assertEqual(self.core.crosspoint(1, 1), float("-inf"))

    def test_gain_set_input_trim_and_gain_telemetry(self):
        self.disp.dispatch("gain.set", {"channel": 0, "gain_db": -2.0, "stage": "input"})
        gains = self.link.telemetry_for("audio.gain")
        self.assertTrue(gains)
        self.assertEqual(gains[-1], {"channel": 0, "gain_db": -2.0, "stage": "input"})

    def test_gain_set_output_stage(self):
        self.disp.dispatch("gain.set", {"channel": 1, "gain_db": 1.5, "stage": "output"})
        gains = self.link.telemetry_for("audio.gain")
        self.assertEqual(gains[-1]["stage"], "output")

    def test_gain_set_mute_the_mic(self):
        # "mute the mic" -> gain.set with mute:true on an input. Must mutate the
        # matrix (revision bumps) and not raise.
        r0 = self.core.matrix_revision()
        self.disp.dispatch("gain.set", {"channel": 0, "mute": True})
        self.assertGreater(self.core.matrix_revision(), r0)
        # The mute rides a DISTINCT audio.gain payload the HUD accepts:
        # {channel, muted, stage} — never a gain_db=null frame (parseNexusGain
        # rejected those wholesale, so mutes were invisible on the panel).
        gains = self.link.telemetry_for("audio.gain")
        self.assertTrue(gains, "a mute must emit an audio.gain telemetry")
        self.assertEqual(gains[-1], {"channel": 0, "muted": True, "stage": "input"})
        self.assertNotIn("gain_db", gains[-1])

    def test_gain_set_unmute_emits_muted_false(self):
        self.disp.dispatch("gain.set", {"channel": 0, "mute": True})
        self.disp.dispatch("gain.set", {"channel": 0, "mute": False})
        gains = self.link.telemetry_for("audio.gain")
        self.assertEqual(gains[-1], {"channel": 0, "muted": False, "stage": "input"})

    def test_monitor_set_assigns_and_routes(self):
        self.disp.dispatch("monitor.set", {"in": 2, "out": 0, "on": True})
        # The direct monitor route is opened at unity.
        self.assertAlmostEqual(self.core.crosspoint(2, 0), 0.0, places=4)
        self.assertTrue(self.link.telemetry_for("audio.routes"))

    def test_monitor_set_off_clears_route(self):
        self.disp.dispatch("monitor.set", {"in": 2, "out": 0, "on": True})
        self.disp.dispatch("monitor.set", {"in": 2, "out": 0, "on": False})
        self.assertEqual(self.core.crosspoint(2, 0), float("-inf"))

    def test_monitor_measure_reports_null_rtt(self):
        # Device-gated: must report measured_rtt_ms=None, never fabricate.
        self.disp.dispatch("monitor.measure", {})
        routes = self.link.telemetry_for("audio.routes")
        self.assertTrue(routes)
        self.assertIsNone(routes[-1]["measured_rtt_ms"])

    def test_state_get_emits_full_snapshot(self):
        self.disp.dispatch("route.set", {"in": 0, "out": 0, "gain_db": -1.0})
        self.link.lines.clear()
        self.disp.dispatch("state.get", {})
        routes = self.link.telemetry_for("audio.routes")
        self.assertTrue(routes)
        snap = routes[-1]
        self.assertEqual(snap["inputs"], self.core.inputs())
        self.assertEqual(snap["outputs"], self.core.outputs())
        self.assertIn("revision", snap)
        self.assertIsNone(snap["measured_rtt_ms"])

    def test_unknown_op_logs_and_does_not_crash(self):
        self.disp.dispatch("frobnicate", {"op": "frobnicate"})
        self.assertTrue(any("unknown op" in l for l in self.link.logs()))

    def test_emit_routes_payload_is_json_safe(self):
        self.disp.dispatch("route.set", {"in": 0, "out": 0, "gain_db": 0.0})
        for ln in self.link.lines:
            json.dumps(ln)  # the full wire line must serialize


# --------------------------------------------------------------------------- #
# Native-core: gain / index clamping (bad ops rejected, dispatcher survives).
# --------------------------------------------------------------------------- #
class TestClamping(unittest.TestCase):
    def setUp(self):
        self.core = _load_core_or_skip()
        self.link = FakeLink()
        self.disp = nexus.OpDispatcher(self.core, self.link)
        self.addCleanup(self.core.close)

    def test_crosspoint_above_max_gain_is_rejected(self):
        # +12 dB is the ceiling; +13 must be rejected by the core.
        with self.assertRaises(nexus.NexusCoreError):
            self.core.set_crosspoint(0, 0, 13.0)

    def test_crosspoint_at_max_gain_ok(self):
        self.core.set_crosspoint(0, 0, 12.0)
        self.assertAlmostEqual(self.core.crosspoint(0, 0), 12.0, places=4)

    def test_out_of_range_index_rejected(self):
        with self.assertRaises(nexus.NexusCoreError):
            self.core.set_crosspoint(99, 0, 0.0)

    def test_dispatch_swallows_bad_route_op(self):
        # A route.set with an out-of-range index must log, not raise/crash.
        self.disp.dispatch("route.set", {"in": 99, "out": 0, "gain_db": 0.0})
        self.assertTrue(any("route.set failed" in l for l in self.link.logs()))

    def test_dispatch_swallows_nan_gain(self):
        # NaN is neither the -inf sentinel nor finite<=+12: the core rejects it
        # and the dispatcher logs rather than dying.
        self.disp.dispatch("route.set", {"in": 0, "out": 0, "gain_db": float("nan")})
        self.assertTrue(any("route.set failed" in l for l in self.link.logs()))

    def test_input_trim_nan_rejected(self):
        with self.assertRaises(nexus.NexusCoreError):
            self.core.set_input_trim(0, float("nan"))


# --------------------------------------------------------------------------- #
# Native-core: preset save (scratch) + load (presets) round-trip through ops.
# --------------------------------------------------------------------------- #
class TestPresetOps(unittest.TestCase):
    def setUp(self):
        self.core = _load_core_or_skip()
        self.link = FakeLink()
        self.disp = nexus.OpDispatcher(self.core, self.link)
        self.addCleanup(self.core.close)

    def test_preset_save_writes_scratch_and_load_replays(self):
        # Set a couple of routes, save them, clear, then load the saved file back
        # and assert the matrix is restored. Save goes to SCRATCH (fs_write);
        # load reads PRESETS (fs_read), so we redirect both at the module dirs
        # for a hermetic round-trip.
        self.core.set_crosspoint(0, 0, -3.0)
        self.core.set_crosspoint(1, 2, -6.0)

        with tempfile.TemporaryDirectory() as td:
            scratch = Path(td) / "scratch"
            presets = Path(td) / "presets"
            scratch.mkdir()
            presets.mkdir()
            orig_scratch, orig_presets = nexus.SCRATCH_DIR, nexus.PRESETS_DIR
            nexus.SCRATCH_DIR = scratch
            nexus.PRESETS_DIR = presets
            try:
                self.disp.dispatch("preset.save", {"name": "vocal"})
                saved = scratch / "vocal.toml"
                self.assertTrue(saved.exists(), "save must write scratch/<name>.toml")

                # Promote the saved file into presets/ (the manual curation step
                # the app documents: presets/ is read-only to the app) and clear
                # the matrix.
                (presets / "vocal.toml").write_text(saved.read_text(), encoding="utf-8")
                self.core.set_crosspoint(0, 0, float("-inf"))
                self.core.set_crosspoint(1, 2, float("-inf"))
                self.assertEqual(self.core.crosspoint(0, 0), float("-inf"))

                self.disp.dispatch("preset.load", {"name": "vocal"})
            finally:
                nexus.SCRATCH_DIR, nexus.PRESETS_DIR = orig_scratch, orig_presets

        # The loaded preset restored both crosspoints identically.
        self.assertAlmostEqual(self.core.crosspoint(0, 0), -3.0, places=4)
        self.assertAlmostEqual(self.core.crosspoint(1, 2), -6.0, places=4)

    def test_preset_load_missing_logs_not_crashes(self):
        with tempfile.TemporaryDirectory() as td:
            orig = nexus.PRESETS_DIR
            nexus.PRESETS_DIR = Path(td)
            try:
                self.disp.dispatch("preset.load", {"name": "nope"})
            finally:
                nexus.PRESETS_DIR = orig
        self.assertTrue(any("preset.load failed" in l for l in self.link.logs()))

    def test_preset_load_rejects_traversal_name(self):
        # A traversal preset name must be rejected before any filesystem touch.
        self.disp.dispatch("preset.load", {"name": "../../etc/hosts"})
        self.assertTrue(any("preset.load failed" in l for l in self.link.logs()))

    def test_preset_save_confines_to_scratch(self):
        with tempfile.TemporaryDirectory() as td:
            scratch = Path(td) / "scratch"
            scratch.mkdir()
            orig = nexus.SCRATCH_DIR
            nexus.SCRATCH_DIR = scratch
            try:
                self.disp.dispatch("preset.save", {"name": "../escape"})
            finally:
                nexus.SCRATCH_DIR = orig
            # Nothing was written outside the scratch dir.
            self.assertEqual(list(scratch.glob("*.toml")), [])
            self.assertFalse((Path(td) / "escape.toml").exists())
        self.assertTrue(any("preset.save failed" in l for l in self.link.logs()))


# --------------------------------------------------------------------------- #
# Native-core: telemetry fold payload shapes match the SPEC §6 topics.
# --------------------------------------------------------------------------- #
class TestTelemetryFold(unittest.TestCase):
    def setUp(self):
        self.core = _load_core_or_skip()
        self.link = FakeLink()
        self.addCleanup(self.core.close)

    def test_levels_payload_shape(self):
        nexus._emit_levels(self.core, self.link)
        payloads = self.link.telemetry_for("audio.levels")
        self.assertTrue(payloads)
        p = payloads[-1]
        # {ch: [{peak_dbfs, rms_dbfs}], lufs_m, lufs_s, lufs_i}
        self.assertEqual(len(p["ch"]), self.core.inputs())
        for entry in p["ch"]:
            self.assertIn("peak_dbfs", entry)
            self.assertIn("rms_dbfs", entry)
        for k in ("lufs_m", "lufs_s", "lufs_i"):
            self.assertIn(k, p)
        json.dumps(p)  # JSON-safe (silence folded to None)

    def test_spectrum_payload_is_96_bands(self):
        nexus._emit_spectrum(self.core, self.link)
        payloads = self.link.telemetry_for("audio.spectrum")
        self.assertTrue(payloads)
        bands = payloads[-1]["bands"]
        self.assertEqual(len(bands), 96)
        json.dumps(payloads[-1])

    def test_routes_payload_shape(self):
        disp = nexus.OpDispatcher(self.core, self.link)
        disp.emit_routes()
        payloads = self.link.telemetry_for("audio.routes")
        self.assertTrue(payloads)
        p = payloads[-1]
        for k in ("matrix", "inputs", "outputs", "revision", "measured_rtt_ms"):
            self.assertIn(k, p)
        self.assertIsInstance(p["matrix"], list)
        self.assertNotIn("route", p, "crosspoints must ride the HUD's 'matrix' wire key")
        self.assertIsNone(p["measured_rtt_ms"])


# --------------------------------------------------------------------------- #
# Native-core: the ctypes <-> cdylib contract (mirror of the --selftest).
# --------------------------------------------------------------------------- #
class TestCoreContract(unittest.TestCase):
    def setUp(self):
        self.core = _load_core_or_skip()
        self.addCleanup(self.core.close)

    def test_geometry_matches_defaults(self):
        self.assertEqual(self.core.inputs(), nexus.DEFAULT_INPUTS)
        self.assertEqual(self.core.outputs(), nexus.DEFAULT_OUTPUTS)

    def test_crosspoint_set_get_clear(self):
        self.core.set_crosspoint(0, 0, -3.0)
        self.assertAlmostEqual(self.core.crosspoint(0, 0), -3.0, places=4)
        self.core.set_crosspoint(0, 0, float("-inf"))
        self.assertEqual(self.core.crosspoint(0, 0), float("-inf"))

    def test_spectrum_band_count(self):
        self.assertEqual(len(self.core.spectrum()), 96)

    def test_loudness_returns_triplet(self):
        m, s, i = self.core.loudness()
        self.assertTrue(all(isinstance(x, float) for x in (m, s, i)))

    def test_clip_drain_reports_once_then_clears(self):
        # Fresh core: nothing has clipped, so the edge-triggered accumulator
        # drains EMPTY (the ctypes out-array binding round-trips a zero count).
        self.assertEqual(self.core.drain_clips(), [])
        # Drive ONE full-scale block through the realtime path: 0 dBFS sits above
        # the -1 dBFS true-peak ceiling, so the detector fires on channel 0.
        self.core.set_crosspoint(0, 0, 0.0)
        self.core.set_monitor_output(0)
        self.core.process_block([[1.0] * nexus.DEFAULT_BLOCK_FRAMES], out_channels=1)
        clips = self.core.drain_clips()
        self.assertTrue(clips, "a full-scale block must enqueue a clip event")
        self.assertEqual(clips[0][0], 0, "event must carry the input channel index")
        self.assertGreaterEqual(clips[0][1], -1.0, "true-peak must meet the -1 dBFS ceiling")
        # Edge-triggered: each event is delivered exactly once.
        self.assertEqual(self.core.drain_clips(), [])


if __name__ == "__main__":
    unittest.main()
