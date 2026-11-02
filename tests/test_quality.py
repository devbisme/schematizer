# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""The wiring score, and the part-alignment pass judged by it."""

import json
import os

import pytest

from schematizer import render
from schematizer.quality import score_files, score_schematic

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
