"""Hierarchical sheets connect to their parents through matching labels and pins."""

import json
import os
import re
import shutil
import subprocess

import pytest

from schematizer import render
from schematizer.engine.sexp_schematic import A_SIZES, _pick_paper_size, _usable_area
from schematizer.geometry import BBox, Point

DATA = os.path.join(os.path.dirname(__file__), "data")


def _load(case):
    with open(os.path.join(DATA, f"{case}.json")) as f:
        return json.load(f)


def _nested(doc, parent, child):
    """Move sheet `child` from under the root to under sheet `parent`."""

    def move(hier):
        return ["", parent, child] if list(hier) == ["", child] else hier

    doc["sheets"] = [move(s) for s in doc["sheets"]]
    for comp in doc["components"]:
        comp["hiertuple"] = move(comp["hiertuple"])
    return doc


def _labels(text, kind):
    """{(name, x, y)} of the labels of one kind in a .kicad_sch file's text."""
    pattern = rf'\({kind} "([^"]+)"\s+\(shape \w+\)\s+\(at ([\d.-]+) ([\d.-]+)'
    return {(n, float(x), float(y)) for n, x, y in re.findall(pattern, text)}


def _sheets(text):
    """{sheet file: {pin name: (x, y)}} for the sheet symbols in a .kicad_sch file's text."""
    sheets = {}
    for block in re.split(r"\n\s*\(sheet\s*\n", text)[1:]:
        block = block[: block.find("(instances")]
        file = re.search(r'\(property "Sheetfile" "([^"]+)"', block).group(1)
        pins = re.findall(r'\(pin "([^"]+)" \w+\s+\(at ([\d.-]+) ([\d.-]+)', block)
        sheets[file] = {n: (float(x), float(y)) for n, x, y in pins}
    return sheets


def _check_sheet_pins(out):
    """Each sheet pin has a hierarchical label to match inside and a label on it outside."""
    texts = {f: open(os.path.join(out, f)).read() for f in os.listdir(out) if f.endswith(".kicad_sch")}
    checked = 0
    for parent_text in texts.values():
        outside = {(n, x, y) for kind in ("global_label", "hierarchical_label") for n, x, y in _labels(parent_text, kind)}
        for file, pins in _sheets(parent_text).items():
            inside = texts[file]
            hier_names = {n for n, _, _ in _labels(inside, "hierarchical_label")}
            assert set(pins) == hier_names, file
            # The child doesn't also reach those nets by global label.
            assert not hier_names & {n for n, _, _ in _labels(inside, "global_label")}, file
            for name, (x, y) in pins.items():
                assert (name, x, y) in outside, (file, name)
            checked += len(pins)
    return checked


@pytest.mark.parametrize("seed", [1, 2])
def test_sheet_pins_match_labels_inside_and_out(tmp_path, seed):
    """A sheet pin with no label inside the sheet, or none on it outside, dangles."""
    out = str(tmp_path / "out")
    render(_load("hier"), filepath=out, top_name="sch", seed=seed)
    assert _check_sheet_pins(out) > 0


def test_nested_sheet_passes_nets_up_through_its_parent(tmp_path):
    """A sheet inside a sheet reaches the root through a pin on each level."""
    out = str(tmp_path / "out")
    render(_nested(_load("hier"), "stage1", "stage2"), filepath=out, top_name="sch", seed=1)
    stage1 = next(f for f in os.listdir(out) if f.endswith("stage1.kicad_sch"))
    assert _sheets(open(os.path.join(out, stage1)).read()), "stage2 should be a sheet inside stage1"
    assert _check_sheet_pins(out) > 0


@pytest.mark.skipif(not shutil.which("kicad-cli"), reason="needs kicad-cli")
@pytest.mark.parametrize("nest", [False, True])
def test_kicad_finds_no_dangling_sheet_pins_or_labels(tmp_path, nest):
    doc = _load("hier")
    if nest:
        doc = _nested(doc, "stage1", "stage2")
    out = tmp_path / "out"
    render(doc, filepath=str(out), top_name="sch", seed=1)
    report = tmp_path / "erc.rpt"
    subprocess.run(
        ["kicad-cli", "sch", "erc", "-o", str(report), str(out / "sch.kicad_sch")],
        check=True, capture_output=True,
    )
    kinds = set(re.findall(r"^\[(\w+)\]", report.read_text(), re.M))
    assert not kinds & {"label_dangling", "pin_not_connected", "hier_label_mismatch"}


def test_paper_leaves_room_for_frame_and_title_block():
    """A drawing as big as a bare A4 page needs A3 once the frame and title block are kept clear."""
    w, h = A_SIZES["A4"]
    mils = 1 / 0.0254
    assert _pick_paper_size(BBox(Point(0, 0), Point(w * mils, h * mils))) == "A3"
    uw, uh = _usable_area("A4")
    assert _pick_paper_size(BBox(Point(0, 0), Point(uw * mils, uh * mils))) == "A4"
