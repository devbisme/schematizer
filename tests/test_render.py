# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""End-to-end tests: generic netlist JSON -> KiCad schematic files."""

import glob
import json
import os

import pytest

from schematizer import render
from schematizer.loader import load_netlist
from schematizer.render import SUPPORTED_TOOLS

DATA = os.path.join(os.path.dirname(__file__), "data")


def _doc(name):
    with open(os.path.join(DATA, name)) as f:
        return json.load(f)


def test_loader_reconstructs_connectivity():
    """The loader rebuilds the exact net connectivity from the document."""
    doc = _doc("flat.json")
    ckt = load_netlist(doc, tool="kicad9")

    # {net_name: sorted[(ref, pin)]} from the reconstructed circuit...
    got = {
        net.name: sorted((p.part.ref, str(p.num)) for p in net.pins)
        for net in ckt.get_nets()
    }
    # ...must equal the document's own connectivity.
    want = {
        net["name"]: sorted((n["ref"], str(n["pin"])) for n in net["nodes"])
        for net in doc["nets"]
    }
    assert got == want


def test_loader_applies_hints():
    """User hints (symtx, stub) survive reconstruction."""
    doc = _doc("flat.json")
    ckt = load_netlist(doc, tool="kicad9")

    r1 = {p.ref: p for p in ckt.parts}["R1"]
    assert getattr(r1, "symtx", "") == "H"

    gnd = {n.name: n for n in ckt.get_nets()}["GND"]
    assert gnd.stub is True


def test_render_flat_writes_schematic(tmp_path):
    out = render(_doc("flat.json"), tool="kicad9", filepath=str(tmp_path))
    files = glob.glob(os.path.join(out, "*.kicad_sch"))
    assert len(files) == 1
    # The output is a non-trivial KiCad schematic.
    text = open(files[0]).read()
    assert "kicad_sch" in text
    assert "lib_symbols" in text


def test_render_hierarchical_writes_sheets(tmp_path):
    out = render(_doc("hier.json"), tool="kicad9", filepath=str(tmp_path),
                 top_name="hier")
    files = sorted(
        os.path.basename(f)
        for f in glob.glob(os.path.join(out, "*.kicad_sch"))
    )
    # Root sheet + one sub-sheet per subcircuit instance.
    assert "hier.kicad_sch" in files
    assert len(files) >= 3


def test_render_accepts_path(tmp_path):
    out = render(os.path.join(DATA, "flat.json"), tool="kicad9",
                 filepath=str(tmp_path))
    assert glob.glob(os.path.join(out, "*.kicad_sch"))


def test_render_rejects_unknown_tool(tmp_path):
    with pytest.raises(ValueError):
        render(_doc("flat.json"), tool="kicad99", filepath=str(tmp_path))


def test_render_rejects_kicad5(tmp_path):
    """KiCad 5 needs the legacy EESCHEMA writer, which this tool doesn't have.

    Guards against it being quietly routed through the modern s-expression
    writer, which would emit a .kicad_sch that KiCad 5 cannot open -- and
    which crashes anyway, since KiCad 5 libraries carry no draw_cmds.
    """
    assert "kicad5" not in SUPPORTED_TOOLS
    with pytest.raises(ValueError, match="KiCad 5"):
        render(_doc("flat.json"), tool="kicad5", filepath=str(tmp_path))
    assert not glob.glob(os.path.join(str(tmp_path), "*.kicad_sch"))


def test_render_rejects_bad_schema(tmp_path):
    bad = _doc("flat.json")
    bad["format"] = "something-else"
    with pytest.raises(ValueError):
        render(bad, tool="kicad9", filepath=str(tmp_path))


def test_render_does_not_import_skidl(tmp_path):
    """The engine is fully vendored: rendering must not pull in skidl.

    Run in a subprocess: some tests in this suite import skidl to build
    circuits, so an in-process ``sys.modules`` check would depend on test
    ordering.
    """
    import subprocess
    import sys

    script = (
        "import sys;"
        "from schematizer import render;"
        f"render({_doc('flat.json')!r}, tool='kicad9', filepath={str(tmp_path)!r});"
        "sys.exit(1 if 'skidl' in sys.modules else 0)"
    )
    proc = subprocess.run([sys.executable, "-c", script], capture_output=True)
    assert proc.returncode == 0, (
        "rendering imported skidl:\n" + proc.stderr.decode()
    )


def test_kicad8_format_toggle(tmp_path):
    """KiCad 8 spells hidden pin numbers differently from KiCad 9/10."""
    render(_doc("flat.json"), tool="kicad8", filepath=str(tmp_path / "k8"),
           top_name="flat")
    render(_doc("flat.json"), tool="kicad9", filepath=str(tmp_path / "k9"),
           top_name="flat")
    k8 = open(os.path.join(str(tmp_path / "k8"), "flat.kicad_sch")).read()
    k9 = open(os.path.join(str(tmp_path / "k9"), "flat.kicad_sch")).read()
    assert "(pin_numbers hide)" in k8
    assert "(pin_numbers hide)" not in k9


def test_render_uses_vendored_power_symbol(tmp_path):
    """A GND net renders via the vendored KiCad power library (no KiCad access)."""
    out = render(_doc("flat.json"), tool="kicad9", filepath=str(tmp_path),
                 top_name="flat")
    text = open(os.path.join(out, "flat.kicad_sch")).read()
    assert "power:GND" in text


def test_explicit_power_symbols_not_duplicated(tmp_path):
    """Explicit GND/VCC power parts are the nets' only power symbols.

    Neither a NetTerminal for the named power net nor a routed 2-pin part on it
    should add a second symbol alongside the user's own.
    """
    out = render(_doc("and_gate.json"), tool="kicad9", filepath=str(tmp_path),
                 top_name="and_gate", seed=1)
    text = open(os.path.join(out, "and_gate.kicad_sch")).read()
    assert text.count('(lib_id "power:GND")') == 1
    assert text.count('(lib_id "power:VCC")') == 1


def _power_symbols(text):
    """(value, x, y) of each power symbol placed in a .kicad_sch file's text."""
    import re

    return re.findall(
        r'\(lib_id "power:([^"]+)"\)\s+\(at ([\d.-]+) ([\d.-]+)', text
    )


def test_explicit_power_symbols_get_power_flag(tmp_path):
    """A netlist's own power symbol is defined as one, so every symbol of its name connects."""
    out = render(_doc("and_gate.json"), tool="kicad9", filepath=str(tmp_path),
                 top_name="and_gate", seed=1, auto_stub=True, erc_max_iterations=0)
    text = open(os.path.join(out, "and_gate.kicad_sch")).read()
    lib = text[text.index("(lib_symbols"):]
    for name in ("GND", "VCC"):
        definition = lib[lib.index(f'(symbol "power:{name}"'):]
        assert definition.split("\n", 2)[1].strip() == "(power)"


def test_auto_stub_puts_no_power_symbol_on_a_power_symbol(tmp_path):
    out = render(_doc("and_gate.json"), tool="kicad9", filepath=str(tmp_path),
                 top_name="and_gate", seed=1, auto_stub=True, erc_max_iterations=0)
    spots = [(x, y) for _, x, y in _power_symbols(open(os.path.join(out, "and_gate.kicad_sch")).read())]
    assert len(spots) == len(set(spots))


@pytest.mark.skipif(not __import__("shutil").which("kicad-cli"), reason="needs kicad-cli")
@pytest.mark.parametrize("seed", [1, 2])
def test_auto_stub_keeps_power_nets_connected(tmp_path, seed):
    """With the netlist's own GND/VCC symbols, KiCad still joins every pin of those nets."""
    import re
    import subprocess

    doc = _doc("and_gate.json")
    out = render(doc, tool="kicad9", filepath=str(tmp_path / "out"),
                 top_name="and_gate", seed=seed, auto_stub=True)
    net_file = tmp_path / "net.net"
    subprocess.run(
        ["kicad-cli", "sch", "export", "netlist", "--format", "kicadsexpr",
         "-o", str(net_file), os.path.join(out, "and_gate.kicad_sch")],
        check=True, capture_output=True,
    )
    text = net_file.read_text()
    drawn = {}
    for block in re.split(r"\(net \(code", text[text.index("(nets"):])[1:]:
        nodes = re.findall(r'\(node \(ref "([^"]+)"\) \(pin "([^"]+)"\)', block)
        for node in nodes:
            drawn[node] = block
    for net in doc["nets"]:
        if net["name"] not in ("GND", "VCC"):
            continue
        pins = [(n["ref"], str(n["pin"])) for n in net["nodes"] if not n["ref"].startswith("#")]
        assert len({id(drawn[p]) for p in pins}) == 1, net["name"]
