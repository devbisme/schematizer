# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""Subcircuits flattened onto their parent's page keep their routed wires."""

import glob
import json
import os

import pytest

from schematizer import render
from schematizer.engine import gen_schematic

DATA = os.path.join(os.path.dirname(__file__), "data")


@pytest.fixture
def doc():
    with open(os.path.join(DATA, "span_hier.json")) as f:
        return json.load(f)


def _count(directory, pattern, text):
    return sum(
        open(path).read().count(text)
        for path in glob.glob(os.path.join(directory, pattern))
    )


def test_top_sheet_keeps_wires_of_flattened_children(doc, tmp_path):
    render(doc, tool="kicad9", filepath=str(tmp_path), flatness=1.0, seed=1)
    assert _count(tmp_path, "*.kicad_sch", "(wire") > 0


def test_auto_stub_keeps_wires_in_flattened_small_subcircuits(doc, tmp_path, monkeypatch):
    # Snapping two-pin parts onto their neighbors stubs nets on its own account.
    monkeypatch.setattr(gen_schematic, "_snap_two_pin_parts", lambda node: None)
    render(
        doc, tool="kicad9", format="svg", filepath=str(tmp_path),
        flatness=1.0, seed=1, auto_stub=True,
    )
    assert _count(tmp_path, "*.svg", "<line") > 0


def test_auto_stub_still_labels_small_subcircuits_on_their_own_sheets(doc, tmp_path):
    render(
        doc, tool="kicad9", format="svg", filepath=str(tmp_path),
        flatness=0.0, seed=1, auto_stub=True,
    )
    assert _count(tmp_path, "*.svg", "<line") == 0
