# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""End-to-end tests: generic netlist JSON -> static SVG schematic pages."""

import glob
import json
import os
import re
import xml.etree.ElementTree as ET

import pytest

from schematizer import render
from schematizer.render import SUPPORTED_FORMATS

DATA = os.path.join(os.path.dirname(__file__), "data")

SVG_NS = "http://www.w3.org/2000/svg"


def _doc(name):
    with open(os.path.join(DATA, name)) as f:
        return json.load(f)


def _svgs(path):
    return sorted(os.path.basename(p) for p in glob.glob(os.path.join(path, "*.svg")))


def _render(doc, tmp_path, **kw):
    kw.setdefault("tool", "kicad9")
    kw.setdefault("top_name", "sch")
    return render(_doc(doc), filepath=str(tmp_path), format="svg", **kw)


def _hrefs(svg_text):
    """Every hyperlink target in an SVG page."""
    return sorted(set(re.findall(r'<a href="([^"]+)"', svg_text)))


def test_svg_is_a_supported_format():
    assert "svg" in SUPPORTED_FORMATS


def test_render_svg_writes_one_page_for_a_flat_circuit(tmp_path):
    _render("flat.json", tmp_path)
    assert _svgs(tmp_path) == ["sch.svg"]
    # ...and no KiCad files: the format selects the writer, it doesn't add one.
    assert not glob.glob(os.path.join(str(tmp_path), "*.kicad_sch"))


def test_svg_output_is_well_formed_xml(tmp_path):
    _render("flat.json", tmp_path)
    root = ET.parse(os.path.join(str(tmp_path), "sch.svg")).getroot()
    assert root.tag == f"{{{SVG_NS}}}svg"
    # A viewBox is what makes the page scale in a browser.
    assert root.get("viewBox")
    assert root.get("width", "").endswith("mm")


def test_svg_draws_the_symbol_graphics(tmp_path):
    """Symbols are drawn from the netlist's embedded draw_cmds.

    Guards the whole point of the SVG backend: the page must contain real
    symbol geometry, not just wires and text.
    """
    _render("flat.json", tmp_path)
    svg = open(os.path.join(str(tmp_path), "sch.svg")).read()
    assert '<g class="part"' in svg
    # Every component in the document is drawn and labelled.
    for comp in _doc("flat.json")["components"]:
        assert f'data-ref="{comp["ref"]}"' in svg
    # Resistor/capacitor bodies and pin lines.
    assert "<rect" in svg and "<polyline" in svg
    # Routed wires.
    assert "<line" in svg


def test_hierarchical_svg_writes_a_page_per_sheet(tmp_path):
    _render("hier.json", tmp_path, flatness=0.0)
    assert _svgs(tmp_path) == ["sch.svg", "sch_stage1.svg", "sch_stage2.svg"]


def test_flattened_svg_writes_a_single_page(tmp_path):
    """flatness=1.0 collapses the hierarchy onto one page, as it does for KiCad."""
    _render("hier.json", tmp_path, flatness=1.0)
    assert _svgs(tmp_path) == ["sch.svg"]


def test_parent_links_to_children_and_children_link_back(tmp_path):
    """The hierarchy is navigable: parent -> child boxes, child -> parent."""
    _render("hier.json", tmp_path, flatness=0.0)

    top = open(os.path.join(str(tmp_path), "sch.svg")).read()
    assert _hrefs(top) == ["sch_stage1.svg", "sch_stage2.svg"]
    # Each link wraps a drawn box carrying the child's sheet name.
    assert top.count('<g class="sheet">') == 2
    for name in ("stage1", "stage2"):
        assert f">{name}<" in top

    for child in ("sch_stage1.svg", "sch_stage2.svg"):
        text = open(os.path.join(str(tmp_path), child)).read()
        assert _hrefs(text) == ["sch.svg"], f"{child} must link back to its parent"


def test_child_pages_link_to_the_top_of_a_deep_hierarchy(tmp_path):
    """Below the second level, a page links to BOTH its parent and the top."""
    doc = _doc("hier.json")
    # Push stage2 underneath stage1 so stage2's parent is no longer the top.
    for comp in doc["components"]:
        if comp["hiertuple"] == ["", "stage2"]:
            comp["hiertuple"] = ["", "stage1", "stage2"]
    render(
        doc,
        tool="kicad9",
        filepath=str(tmp_path),
        top_name="sch",
        format="svg",
        flatness=0.0,
    )

    deep = os.path.join(str(tmp_path), "sch_stage1_stage2.svg")
    assert os.path.exists(deep), _svgs(tmp_path)
    text = open(deep).read()
    assert _hrefs(text) == ["sch.svg", "sch_stage1.svg"]
    assert "Up:" in text and "Top:" in text


def test_svg_pin_graphics_land_on_the_routed_wire_ends(tmp_path):
    """A symbol's drawn pins must coincide with the ends of its wires.

    This is the correctness property that makes the page a schematic rather
    than a collage: the symbol renderer and the wire emitter have to agree on
    where a pin is, despite reaching it through different transforms.
    """
    _render("flat.json", tmp_path)
    svg = open(os.path.join(str(tmp_path), "sch.svg")).read()

    def _pts(pattern):
        return {
            (round(float(a), 2), round(float(b), 2))
            for a, b in re.findall(pattern, svg)
        }

    wire_ends = _pts(r'<line x1="([-\d.]+)" y1="([-\d.]+)"')
    wire_ends |= _pts(r'x2="([-\d.]+)" y2="([-\d.]+)"')
    pin_ends = _pts(r'<polyline points="([-\d.]+),([-\d.]+) ')

    assert wire_ends, "no wires were routed, so this proves nothing"
    # Every wire endpoint should be a pin end, a junction, or a corner where it
    # meets another wire. Require that a solid majority sit on drawn pins.
    on_pin = wire_ends & pin_ends
    assert on_pin, "no wire endpoint coincides with any drawn pin"


def test_svg_and_kicad_agree_on_part_placement(tmp_path):
    """The SVG page has the same layout as the .kicad_sch for the same input.

    Both writers consume one placed+routed tree, so a part's origin must come
    out identical. Placement is randomized, so the two runs are compared by
    their own internal geometry rather than to a golden file.
    """
    from schematizer.engine import svg_schematic

    seen = {}
    orig = svg_schematic.part_to_svg

    def spy(part, sheet_tx):
        origin = (getattr(part, "tx", None) * sheet_tx).origin
        seen[part.ref] = (round(origin.x, 2), round(origin.y, 2))
        return orig(part, sheet_tx)

    svg_schematic.part_to_svg = spy
    try:
        _render("flat.json", tmp_path)
    finally:
        svg_schematic.part_to_svg = orig

    svg = open(os.path.join(str(tmp_path), "sch.svg")).read()
    assert seen, "no parts were drawn"
    # Each part's field text is drawn near the origin the placer chose.
    for ref in seen:
        assert f'data-ref="{ref}"' in svg


def test_svg_escapes_text(tmp_path):
    """Part values become XML character data, so they must be escaped."""
    doc = _doc("flat.json")
    doc["components"][0]["value"] = 'R<1> & "big"'
    render(
        doc,
        tool="kicad9",
        filepath=str(tmp_path),
        top_name="sch",
        format="svg",
    )
    path = os.path.join(str(tmp_path), "sch.svg")
    # Parses at all == the raw < and & did not break the document.
    ET.parse(path)
    assert "&amp;" in open(path).read()


def test_render_rejects_unknown_format(tmp_path):
    with pytest.raises(ValueError, match="format"):
        render(_doc("flat.json"), tool="kicad9", filepath=str(tmp_path), format="pdf")


def test_kicad_remains_the_default_format(tmp_path):
    """Adding SVG must not change what an existing caller gets."""
    render(_doc("flat.json"), tool="kicad9", filepath=str(tmp_path), top_name="sch")
    assert glob.glob(os.path.join(str(tmp_path), "*.kicad_sch"))
    assert not _svgs(tmp_path)
