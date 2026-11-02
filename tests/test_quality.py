# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""The wiring score, and the part-alignment pass judged by it."""

import json
import os

import pytest

from schematizer import RoutingFailure, render
from schematizer.engine.sch_node import SchNode
from schematizer.quality import score_files, score_schematic
from schematizer.render import LAYOUT_BEND_COST

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
        render(doc, tool="kicad9", filepath=str(out), top_name="sch", seed=seed, align_parts=align)
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
    score = score_files(str(path))
    return score["wirelength"] + LAYOUT_BEND_COST * score["bends"]


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
