# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""Tests for labels-only mode (``render(..., mode="labels")``).

The fixture ``labels_case.json`` has a 3-unit op-amp (LM358) whose units are
wired from two subcircuit sheets, a no-connect pin, PWR_FLAGs, a part field
(MPN) and a net class.
"""

import glob
import json
import os
import shutil
import subprocess

import pytest
from simp_sexp import Sexp

from schematizer import render
from schematizer.engine import sexp_schematic as sx
from schematizer.loader import load_netlist

DATA = os.path.join(os.path.dirname(__file__), "data")
TOP = "labels_case"


def _doc():
    with open(os.path.join(DATA, "labels_case.json")) as f:
        return json.load(f)


def _render(tmp_path, tool="kicad10"):
    render(_doc(), tool=tool, filepath=str(tmp_path), top_name=TOP, mode="labels")
    return {
        os.path.basename(f): Sexp(open(f).read())
        for f in glob.glob(os.path.join(str(tmp_path), "*.kicad_sch"))
    }


def _top(sexp, tag):
    """Top-level elements of a sheet with the given tag."""
    return [x for x in sexp if isinstance(x, list) and x and x[0] == tag]


def _field(node, tag):
    for x in node:
        if isinstance(x, list) and x and x[0] == tag:
            return x
    return None


def _prop(sym, name):
    for x in sym:
        if isinstance(x, list) and x[:2] == ["property", name]:
            return x[2]
    return None


def test_files_and_hierarchy(tmp_path):
    sheets = _render(tmp_path)
    assert set(sheets) == {
        f"{TOP}.kicad_sch",
        f"{TOP}_top1.kicad_sch",
        f"{TOP}_buffer1.kicad_sch",
        f"{TOP}_buffer2.kicad_sch",
    }
    root = sheets[f"{TOP}.kicad_sch"]
    root_uuid = _field(root, "uuid")[1]
    blocks = _top(root, "sheet")
    assert len(blocks) == 3

    # Each child's symbols carry the two-level instance path /root/sheet.
    for block in blocks:
        sheet_uuid = _field(block, "uuid")[1]
        fname = _prop(block, "Sheetfile")
        for sym in _top(sheets[fname], "symbol"):
            path = sym.search("/symbol/instances/project/path")[0][1]
            assert path == f"/{root_uuid}/{sheet_uuid}"
            project = sym.search("/symbol/instances/project")[0][1]
            assert project == TOP


def test_file_version_is_kicad9_format(tmp_path):
    sheets = _render(tmp_path)
    for sexp in sheets.values():
        assert _field(sexp, "version")[1] == sx.file_version("kicad10") == 20250114


def test_multi_unit_part(tmp_path):
    sheets = _render(tmp_path)
    top = sheets[f"{TOP}_top1.kicad_sch"]
    units = sorted(
        _field(s, "unit")[1]
        for s in _top(top, "symbol")
        if _field(s, "lib_id")[1] == "Amplifier_Operational:LM358"
    )
    assert units == [1, 2, 3]

    # Unit sub-symbols use the normal body style (NAME_<unit>_1).
    (lib,) = _top(top, "lib_symbols")
    lm358 = [s for s in lib if isinstance(s, list) and s[:2] == ["symbol", "Amplifier_Operational:LM358"]][0]
    subs = {s[1] for s in lm358 if isinstance(s, list) and s and s[0] == "symbol"}
    assert {"LM358_1_1", "LM358_2_1", "LM358_3_1"} <= subs
    assert not any(n.endswith("_2_2") or n.endswith("_3_3") for n in subs)


def test_every_pin_terminated(tmp_path):
    """One stub per connected pin; one no-connect flag per NC pin."""
    sheets = _render(tmp_path)
    doc = _doc()
    nc_names = {n["name"] for n in doc["nets"] if n.get("nc")}
    connected = nc = 0
    for comp in doc["components"]:
        for pin in comp["pins"]:
            if pin["net"] in nc_names:
                nc += 1
            elif pin["net"] is not None:
                connected += 1
    wires = sum(len(_top(s, "wire")) for s in sheets.values())
    flags = sum(len(_top(s, "no_connect")) for s in sheets.values())
    assert wires == connected
    assert flags == nc == 1


def test_label_kinds(tmp_path):
    sheets = _render(tmp_path)
    glabels = {g[1] for s in sheets.values() for g in _top(s, "global_label")}
    labels = {g[1] for s in sheets.values() for g in _top(s, "label")}
    power = {
        _prop(sym, "Value")
        for s in sheets.values()
        for sym in _top(s, "symbol")
        if _field(sym, "lib_id")[1].startswith("power:") and _prop(sym, "Reference").startswith("#PWR")
    }
    assert power == {"+5V", "GND"}
    assert {"VIN_A", "VIN_B"} <= glabels  # J1 (top1) to R1/R2 (buffer sheets)
    assert {"OUT_A", "OUT_B"} <= labels  # J1 and U1, both on top1
    assert not ({"+5V", "GND"} & (glabels | labels))

    # Power-symbol references are unique across the whole design.
    refs = [
        _prop(sym, "Reference")
        for s in sheets.values()
        for sym in _top(s, "symbol")
        if _prop(sym, "Reference").startswith("#PWR")
    ]
    assert len(refs) == len(set(refs))


def test_part_fields_written(tmp_path):
    sheets = _render(tmp_path)
    mpns = {
        _prop(sym, "Reference"): _prop(sym, "MPN")
        for s in sheets.values()
        for sym in _top(s, "symbol")
    }
    assert mpns["R1"] == "RC0603FR-0710KL"
    assert mpns["U1"] == "LM358DR"


def test_loader_reads_fields():
    ckt = load_netlist(_doc())
    parts = {p.ref: p for p in ckt.parts}
    assert parts["R1"].fields == {"MPN": "RC0603FR-0710KL"}


def test_labels_rejects_svg(tmp_path):
    with pytest.raises(ValueError):
        render(_doc(), tool="kicad10", filepath=str(tmp_path), format="svg", mode="labels")


def test_unknown_mode_rejected(tmp_path):
    with pytest.raises(ValueError):
        render(_doc(), tool="kicad10", filepath=str(tmp_path), mode="bogus")


def _kicad_cli():
    return os.environ.get("SCHEMATIZER_KICAD_CLI") or shutil.which("kicad-cli")


@pytest.mark.skipif(not _kicad_cli(), reason="kicad-cli not available")
def test_kicad_netlist_and_erc(tmp_path):
    """KiCad's netlist matches the document exactly and ERC has no errors."""
    _render(tmp_path)
    cli = _kicad_cli()
    top = os.path.join(str(tmp_path), f"{TOP}.kicad_sch")
    net_file = os.path.join(str(tmp_path), "out.net")
    erc_file = os.path.join(str(tmp_path), "erc.json")
    subprocess.run(
        [cli, "sch", "export", "netlist", "--format", "kicadsexpr", "-o", net_file, top],
        check=True,
        capture_output=True,
    )
    subprocess.run(
        [cli, "sch", "erc", "--format", "json", "--severity-error", "-o", erc_file, top],
        check=False,
        capture_output=True,
    )

    # KiCad leaves "#" references (power symbols, PWR_FLAGs) out of netlists.
    want = set()
    for n in _doc()["nets"]:
        if n.get("nc"):
            continue
        nodes = frozenset((nd["ref"], nd["pin"]) for nd in n["nodes"] if not nd["ref"].startswith("#"))
        if nodes:
            want.add(nodes)
    got = set()
    for net in Sexp(open(net_file).read()).search("/export/nets/net"):
        name = _field(net, "name")[1]
        nodes = frozenset(
            (_field(nd, "ref")[1], str(_field(nd, "pin")[1]))
            for nd in net
            if isinstance(nd, list) and nd and nd[0] == "node"
        )
        if name.startswith("unconnected-") and len(nodes) == 1:
            continue
        got.add(nodes)
    assert got == want

    erc = json.load(open(erc_file))
    errors = [v for s in erc.get("sheets", []) for v in s.get("violations", []) if v["severity"] == "error"]
    assert errors == []
