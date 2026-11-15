"""check_netlist compares the schematic KiCad reads with the netlist it was drawn from."""

import glob
import json
import os
import re
import shutil

import pytest

from schematizer import NetlistMismatch, cli, netcheck, render
from schematizer.netcheck import check_netlist, compare_nets, source_nets

DATA = os.path.join(os.path.dirname(__file__), "data")
needs_kicad = pytest.mark.skipif(not shutil.which("kicad-cli"), reason="needs kicad-cli")


def _load(case):
    with open(os.path.join(DATA, f"{case}.json")) as f:
        return json.load(f)


def _net(*pins):
    return frozenset(tuple(pin.split(".")) for pin in pins)


SOURCE = {"A": _net("R1.1", "R2.1", "U1.3"), "B": _net("R1.2", "C1.1"), "C": _net("C1.2", "C2.2")}


class TestCompareNets:
    def test_same_nets_match(self):
        assert compare_nets(SOURCE, list(SOURCE.values())) == []

    def test_short_between_nets(self):
        drawn = [SOURCE["A"] | SOURCE["B"], SOURCE["C"]]
        assert compare_nets(SOURCE, drawn) == ["A is shorted to B"]

    def test_pin_left_off(self):
        drawn = [_net("R1.1", "R2.1"), SOURCE["B"], SOURCE["C"]]
        assert compare_nets(SOURCE, drawn) == ["A is in 2 pieces (U1.3 not connected to the rest)"]

    def test_net_not_drawn(self):
        problems = compare_nets(SOURCE, [SOURCE["A"], SOURCE["B"]])
        assert problems == ["none of the pins of C are connected (C1.2, C2.2)"]

    def test_short_to_a_pin_that_should_be_unconnected(self):
        drawn = [SOURCE["A"] | _net("X1.1"), SOURCE["B"], SOURCE["C"]]
        assert compare_nets(SOURCE, drawn) == ["A is shorted to X1.1"]

    def test_connection_the_netlist_does_not_have(self):
        drawn = list(SOURCE.values()) + [_net("X1.1", "X2.1")]
        assert compare_nets(SOURCE, drawn) == ["X1.1, X2.1 are connected but shouldn't be"]


def test_source_nets_skip_what_kicad_leaves_out():
    doc = {
        "nets": [
            {"name": "GND", "nodes": [{"ref": "C1", "pin": 2}, {"ref": "#PWR1", "pin": "1"}, {"ref": "C2", "pin": "2"}]},
            {"name": "LONE", "nodes": [{"ref": "R1", "pin": "1"}]},
            {"name": "NC", "nc": True, "nodes": [{"ref": "U1", "pin": "4"}, {"ref": "U1", "pin": "5"}]},
        ]
    }
    assert source_nets(doc) == {"GND": _net("C1.2", "C2.2")}


def test_mismatch_message_lists_a_few_nets():
    error = NetlistMismatch([f"N{i} is shorted to M{i}" for i in range(12)])
    assert "N0 is shorted to M0" in str(error)
    assert "and 4 more" in str(error)
    assert len(error.problems) == 12


@needs_kicad
@pytest.mark.parametrize("options", [{}, {"mode": "labels"}, {"flatness": 1.0}, {"auto_stub": True}])
def test_generated_schematics_pass(tmp_path, options):
    assert render(_load("hier"), filepath=str(tmp_path), seed=1, check_netlist=True, **options)


@needs_kicad
def test_a_removed_wire_is_caught(tmp_path):
    doc = _load("and_gate")
    render(doc, filepath=str(tmp_path), top_name="sch", seed=1)
    top = str(tmp_path / "sch.kicad_sch")
    check_netlist(doc, top)

    text = open(top).read()
    for match in re.finditer(r"\n\s*\(wire\b", text):
        # Drop each wire in turn until one changes the connections.
        start = match.start()
        depth, end = 0, start
        for end in range(text.index("(", start), len(text)):
            depth += {"(": 1, ")": -1}.get(text[end], 0)
            if depth == 0:
                break
        with open(top, "w") as f:
            f.write(text[:start] + text[end + 1 :])
        try:
            check_netlist(doc, top)
        except NetlistMismatch as error:
            assert error.problems
            return
    pytest.fail("removing a wire never changed the netlist")


@pytest.mark.python_engine
def test_tries_check_only_the_kept_layout(monkeypatch, tmp_path):
    checked = []
    monkeypatch.setattr(netcheck, "check_netlist", lambda doc, path: checked.append(path))
    out = str(tmp_path / "out")
    render(_load("and_gate"), filepath=out, top_name="sch", seed=1, tries=3, check_netlist=True)
    assert checked == [os.path.join(out, "sch.kicad_sch")]


def test_not_checked_unless_asked(monkeypatch, tmp_path):
    checked = []
    monkeypatch.setattr(netcheck, "check_netlist", lambda doc, path: checked.append(path))
    monkeypatch.setattr(netcheck.subprocess, "run", lambda *a, **k: pytest.fail("ran a subprocess"))
    render(_load("and_gate"), filepath=str(tmp_path), seed=1)
    assert checked == []


def test_svg_cannot_be_checked(tmp_path):
    with pytest.raises(ValueError, match="check_netlist"):
        render(_load("and_gate"), filepath=str(tmp_path), format="svg", check_netlist=True)


@pytest.mark.python_engine
def test_missing_kicad_cli_is_an_error(monkeypatch, tmp_path):
    monkeypatch.setattr(netcheck.shutil, "which", lambda name: None)
    with pytest.raises(RuntimeError, match="kicad-cli"):
        render(_load("and_gate"), filepath=str(tmp_path), seed=1, check_netlist=True)
    # The files are still there to look at.
    assert glob.glob(str(tmp_path / "*.kicad_sch"))


def test_cli_reports_a_mismatch_and_exits_1(monkeypatch, tmp_path, capsys):
    def failing_render(netlist, **kwargs):
        assert kwargs["check_netlist"] is True
        raise NetlistMismatch(["A is shorted to B"])

    monkeypatch.setattr(cli, "render", failing_render)
    assert cli.main(["netlist.json", "-o", str(tmp_path), "--check-netlist"]) == 1
    assert "A is shorted to B" in capsys.readouterr().err
