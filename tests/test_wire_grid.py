"""Routed wires stay on the 50-mil grid."""

import glob
import json
import os
import re

import pytest

from schematizer import render

DATA = os.path.join(os.path.dirname(__file__), "data")
GRID_MM = 1.27


def _wire_points(out):
    for f in glob.glob(os.path.join(out, "*.kicad_sch")):
        text = open(f).read()
        pts = re.findall(
            r"\(wire\s+\(pts\s+\(xy ([\d.-]+) ([\d.-]+)\)\s+\(xy ([\d.-]+) ([\d.-]+)\)", text
        )
        for x1, y1, x2, y2 in pts:
            yield os.path.basename(f), (float(x1), float(y1)), (float(x2), float(y2))


def _off_grid(v):
    return abs(v / GRID_MM - round(v / GRID_MM)) > 0.01


@pytest.mark.parametrize("case,seed", [("dual_adc_usb", 1), ("dual_adc_usb", 2), ("esp32_audio_board", 1)])
def test_wires_end_on_the_grid(tmp_path, case, seed):
    """The router follows part edges, which can be off the grid; cleanup moves those wires back onto it."""
    with open(os.path.join(DATA, f"{case}.json")) as f:
        doc = json.load(f)
    out = str(tmp_path / "out")
    render(doc, filepath=out, top_name="sch", seed=seed)
    off = [
        (sheet, p1, p2)
        for sheet, p1, p2 in _wire_points(out)
        if any(_off_grid(v) for v in (*p1, *p2))
    ]
    assert not off, off[:5]
