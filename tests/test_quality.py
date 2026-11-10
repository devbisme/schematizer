# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""The wiring score, and the part-alignment pass judged by it."""

import json
import os

import pytest
from simp_sexp import Sexp

from schematizer import RoutingFailure, render
from schematizer.engine.sch_node import SchNode
from schematizer.quality import _field, _fields, score_files, score_overlaps, score_schematic
from schematizer.render import layout_cost

DATA = os.path.join(os.path.dirname(__file__), "data")


def _wire(x1, y1, x2, y2):
    return f"(wire (pts (xy {x1} {y1}) (xy {x2} {y2})) (stroke (width 0)))\n"


def test_an_l_shaped_wire_has_one_bend():
    sch = _wire(0, 0, 5.08, 0) + _wire(5.08, 0, 5.08, 2.54)
    assert score_schematic(sch) == {"segments": 2, "bends": 1, "wirelength": 7.62}


def test_collinear_and_t_joins_are_not_bends():
    # Two collinear pieces of one straight run, plus a branch tee'd off the joint.
    sch = _wire(0, 0, 2.54, 0) + _wire(2.54, 0, 5.08, 0) + _wire(2.54, 0, 2.54, 2.54)
    assert score_schematic(sch)["bends"] == 0


def test_zero_length_wires_are_ignored():
    assert score_schematic(_wire(1, 1, 1, 1))["segments"] == 0


def _bends(doc, tmp_path, align):
    total = 0
    for seed in (1, 2, 3):
        out = tmp_path / f"{align}-{seed}"
        # Force-directed placement is what leaves pins misaligned, so use that.
        render(
            doc,
            tool="kicad9",
            filepath=str(out),
            top_name="sch",
            seed=seed,
            align_parts=align,
            preplace=False,
        )
        total += score_files(str(out))["bends"]
    return total


def test_alignment_straightens_wires(tmp_path):
    """On a circuit with enough wiring for it to matter, aligning pins removes bends."""
    with open(os.path.join(DATA, "and_gate_nopwr.json")) as f:
        doc = json.load(f)
    assert _bends(doc, tmp_path, align=True) < _bends(doc, tmp_path, align=False)


def test_svg_wires_score_like_kicad_wires():
    svg = '<line x1="0" y1="0" x2="5.08" y2="0" /><line x1="5.08" y1="0" x2="5.08" y2="2.54" />'
    assert score_schematic(svg) == {"segments": 2, "bends": 1, "wirelength": 7.62}


def _cost(path):
    return layout_cost(score_files(str(path)))


@pytest.fixture
def and_gate():
    with open(os.path.join(DATA, "and_gate_nopwr.json")) as f:
        return json.load(f)


def test_tries_keeps_the_best_layout(and_gate, tmp_path):
    render(and_gate, filepath=str(tmp_path / "best"), top_name="sch", seed=1, tries=3)
    for i in range(3):
        trial = tmp_path / str(i)
        render(and_gate, filepath=str(trial), top_name="sch", seed=f"1:{i}")
        assert _cost(tmp_path / "best") <= _cost(trial)


def test_tries_is_reproducible(and_gate, tmp_path):
    for run in ("a", "b"):
        render(and_gate, filepath=str(tmp_path / run), top_name="sch", seed=1, tries=2)
    assert (tmp_path / "a/sch.kicad_sch").read_bytes() == (tmp_path / "b/sch.kicad_sch").read_bytes()


def test_tries_works_for_svg(and_gate, tmp_path):
    render(and_gate, format="svg", filepath=str(tmp_path), top_name="sch", seed=1, tries=2)
    assert "<line" in (tmp_path / "sch.svg").read_text()


def test_tries_must_be_positive(and_gate, tmp_path):
    with pytest.raises(ValueError):
        render(and_gate, filepath=str(tmp_path), tries=0)


def test_labels_only_fallback_doesnt_beat_a_routed_layout(and_gate, tmp_path, monkeypatch):
    """A layout with no wires would score best, so it's only used as a last resort."""
    render(and_gate, filepath=str(tmp_path / "routed"), top_name="sch", seed="1:1", auto_stub=True)

    route = SchNode.route
    calls = []

    def fail_first_trial(node, *args, **kwargs):
        calls.append(node)
        if len(calls) <= 2:  # Both routing attempts of the first trial.
            raise RoutingFailure
        return route(node, *args, **kwargs)

    monkeypatch.setattr(SchNode, "route", fail_first_trial)
    render(and_gate, filepath=str(tmp_path / "best"), top_name="sch", seed=1, tries=2, auto_stub=True)
    routed = (tmp_path / "routed/sch.kicad_sch").read_bytes()
    assert (tmp_path / "best/sch.kicad_sch").read_bytes() == routed


def test_every_failed_try_falls_back_as_the_caller_asked(and_gate, tmp_path, monkeypatch):
    route = SchNode.route
    calls = []

    def fail_until_labels_only(node, *args, **kwargs):
        calls.append(node)
        if len(calls) <= 6:  # Two routing attempts for each try, then for the last resort.
            raise RoutingFailure
        return route(node, *args, **kwargs)

    monkeypatch.setattr(SchNode, "route", fail_until_labels_only)
    render(and_gate, filepath=str(tmp_path), top_name="sch", seed=1, tries=2, auto_stub=True)
    assert (tmp_path / "sch.kicad_sch").exists()


_LIB = """(symbol "Device:R"
  (symbol "R_0_1" (rectangle (start -1 -3) (end 1 3)))
  (symbol "R_1_1"
    (pin passive line (at 0 5 270) (length 2) (name "~") (number "1"))
    (pin passive line (at 0 -5 90) (length 2) (name "~") (number "2"))))"""


def _sch(*items):
    return f"(kicad_sch (lib_symbols {_LIB}) {' '.join(items)})"


def _resistor(ref, x, y, angle=0, value_at=None):
    value_at = value_at or (x + 3, y)
    return f"""(symbol (lib_id "Device:R") (at {x} {y} {angle}) (unit 1)
      (property "Reference" "{ref}" (at {x + 3} {y - 2} 0) (effects (font (size 1.27 1.27)) (justify left)))
      (property "Value" "10K" (at {value_at[0]} {value_at[1]} 0) (effects (font (size 1.27 1.27)) (justify left)))
      (property "Footprint" "long hidden text" (at {x} {y} 0) (effects (font (size 1.27 1.27)) (hide yes))))"""


def _label(name, x, y, angle=0):
    return f'(global_label "{name}" (shape bidirectional) (at {x} {y} {angle}))'


def test_separated_parts_have_no_overlaps():
    sch = _sch(_resistor("R1", 0, 0), _resistor("R2", 50, 0))
    assert score_overlaps(sch) == {"part_overlaps": 0, "text_overlaps": 0}


def test_overlapping_parts_are_counted():
    assert score_overlaps(_sch(_resistor("R1", 0, 0), _resistor("R2", 1, 0)))["part_overlaps"] == 1


def test_a_rotated_part_uses_its_rotated_outline():
    # R2's body runs sideways through R1 only when it's rotated.
    upright = _sch(_resistor("R1", 0, 0), _resistor("R2", 5, 0))
    rotated = _sch(_resistor("R1", 0, 0), _resistor("R2", 5, 0, angle=90))
    assert score_overlaps(upright)["part_overlaps"] == 0
    assert score_overlaps(rotated)["part_overlaps"] == 1


def test_text_over_another_part_is_counted():
    sch = _sch(_resistor("R1", 0, 0), _resistor("R2", 50, 0, value_at=(0, 0)))
    assert score_overlaps(sch)["text_overlaps"] == 1


def test_text_against_its_own_part_is_not_counted():
    assert score_overlaps(_sch(_resistor("R1", 0, 0, value_at=(0, 0))))["text_overlaps"] == 0


def test_labels_extend_away_from_their_anchors():
    part = _resistor("R1", 0, 0)
    # A label anchored 5 mm left of the body reaches it when pointing right, not left.
    assert score_overlaps(_sch(part, _label("LONG_NET", -6, 0, 0)))["text_overlaps"] == 1
    assert score_overlaps(_sch(part, _label("LONG_NET", -6, 0, 180)))["text_overlaps"] == 0


def test_hidden_fields_are_ignored():
    sch = _sch(_resistor("R1", 0, 0), _resistor("R2", 50, 0))
    assert "hide" in sch and score_overlaps(sch)["text_overlaps"] == 0


def test_overlaps_raise_the_layout_cost():
    wires = {"wirelength": 10, "bends": 1}
    clean = layout_cost({**wires, "part_overlaps": 0, "text_overlaps": 0})
    assert layout_cost({**wires, "part_overlaps": 0, "text_overlaps": 1}) > clean
    assert layout_cost({**wires, "part_overlaps": 1, "text_overlaps": 0}) > clean


def test_power_symbol_references_are_visible_text():
    far = _resistor("#PWR1", 50, 0, value_at=(50, 20))
    part = _resistor("R1", 53, -2, value_at=(53, 20))
    assert score_overlaps(_sch(part, far))["text_overlaps"] >= 1


def test_a_wire_through_a_parts_text_is_counted():
    part = _resistor("R1", 0, 0)
    wire = "(wire (pts (xy 4 -10) (xy 4 10)))"
    assert score_overlaps(_sch(part, wire))["text_overlaps"] == 2  # Reference and Value.


def test_a_wire_ending_on_a_label_is_not_counted():
    wire = "(wire (pts (xy 20 0) (xy 30 0)))"
    assert score_overlaps(_sch(_label("NET", 20, 0), wire))["text_overlaps"] == 0


def test_pin_names_count_as_part_of_the_symbol():
    lib = _LIB.replace('(name "~")', '(name "A")')
    parts = _resistor("R1", 0, 0, value_at=(50, 0)), _resistor("R2", 50, 0, value_at=(1.1, 4))
    named = f"(kicad_sch (lib_symbols {lib}) {parts[0]} {parts[1]})"
    assert score_overlaps(named)["text_overlaps"] > score_overlaps(_sch(*parts))["text_overlaps"]


def _text_overlaps(doc, tmp_path, **options):
    render(doc, filepath=str(tmp_path), top_name="sch", seed=1, **options)
    return score_files(str(tmp_path))


def test_field_text_moves_off_what_it_collides_with(tmp_path):
    with open(os.path.join(DATA, "and_gate.json")) as f:
        doc = json.load(f)
    fixed = _text_overlaps(doc, tmp_path / "on")
    default = _text_overlaps(doc, tmp_path / "off", place_fields=False)
    assert fixed["text_overlaps"] < default["text_overlaps"]


def test_moving_field_text_leaves_the_wiring_alone(and_gate, tmp_path):
    fixed = _text_overlaps(and_gate, tmp_path / "on")
    default = _text_overlaps(and_gate, tmp_path / "off", place_fields=False)
    for key in ("segments", "bends", "wirelength"):
        assert fixed[key] == default[key]


def test_power_symbol_references_are_hidden(tmp_path):
    with open(os.path.join(DATA, "and_gate.json")) as f:
        doc = json.load(f)
    render(doc, filepath=str(tmp_path), top_name="sch", seed=1)
    sch = Sexp((tmp_path / "sch.kicad_sch").read_text())
    refs = [
        field
        for symbol in _fields(sch, "symbol")
        for field in _fields(symbol, "property")
        if field[1] == "Reference"
    ]
    power = [field for field in refs if field[2].startswith("#")]
    assert power and all(_field(_field(f, "effects"), "hide") for f in power)
    assert not any(
        _field(_field(f, "effects"), "hide") for f in refs if not f[2].startswith("#")
    )


def test_planning_field_text_doesnt_renumber_power_symbols(tmp_path):
    with open(os.path.join(DATA, "and_gate.json")) as f:
        doc = json.load(f)
    render(doc, filepath=str(tmp_path), top_name="sch", seed=1)
    assert '"#PWR1"' in (tmp_path / "sch.kicad_sch").read_text()


def _pin_names_of(doc, path):
    render(doc, filepath=str(path), top_name="sch", seed=1)
    sch = Sexp((path / "sch.kicad_sch").read_text())
    return [_field(sym, "pin_names") for sym in _fields(_field(sch, "lib_symbols"), "symbol")]


def test_kicad_keeps_the_symbols_pin_name_style(tmp_path):
    with open(os.path.join(DATA, "and_gate.json")) as f:
        doc = json.load(f)
    for symbol in doc["symbols"].values():
        symbol["pin_names"] = {"offset": 1.016, "hide": True}
    styles = _pin_names_of(doc, tmp_path)
    assert styles and all(_field(s, "offset")[1] == 1.016 and _field(s, "hide") for s in styles)


def test_documents_without_a_pin_name_style_draw_as_before(tmp_path):
    with open(os.path.join(DATA, "and_gate.json")) as f:
        doc = json.load(f)
    styles = _pin_names_of(doc, tmp_path)
    assert styles and all(_field(s, "offset")[1] == 0 and not _field(s, "hide") for s in styles)
