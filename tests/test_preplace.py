# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""Tests for pre-placement: satellites, the seed, and spreading the seeded parts."""

import json
import os

import pytest

from schematizer import render
from schematizer.engine.gen_schematic import preprocess_circuit, stub_power_nets
from schematizer.engine import constants, place
from schematizer.engine.place import add_anchor_pull_pins, add_placement_bboxes
from schematizer.engine.preplace import (
    attach_satellites,
    find_satellites,
    release_satellites,
    seed_placement,
    spread_parts,
)
from schematizer.loader import load_netlist
from schematizer.quality import score_files

# place() injects GRID etc. into the module when it runs; do the same here.
place.__dict__.update(constants.__dict__)

DATA = os.path.join(os.path.dirname(__file__), "data")


def _doc(name):
    with open(os.path.join(DATA, name)) as f:
        return json.load(f)


@pytest.fixture
def esp32():
    return _doc("esp32_audio_board.json")


def _sheet(doc, sheet):
    """Parts of one sheet and the nets wired between them, ready for placement."""
    circuit = load_netlist(doc)
    stub_power_nets(circuit)
    preprocess_circuit(circuit)
    parts = [part for part in circuit.parts if part.hiertuple[-1] == sheet]
    nets = [
        net
        for net in circuit.nets
        if not net.stub and sum(pin.part in parts for pin in net.pins) > 1
    ]
    add_placement_bboxes(parts)
    return {part.ref: part for part in parts}, nets


def _render(doc, path, **options):
    render(
        doc,
        filepath=str(path),
        top_name="sch",
        auto_stub_fallback="raise",
        **options,
    )
    return score_files(str(path))


def test_satellites_include_chains(esp32):
    """An LED and its series resistor hang off the charger's status pin as a chain."""
    parts, nets = _sheet(esp32, "usb_charger1")
    satellites = find_satellites(list(parts.values()), nets)
    wired_to = {
        sat.ref: host_pin.part.ref
        for sats in satellites.values()
        for sat, _, host_pin in sats
    }
    assert wired_to["R5"] == "D1" and wired_to["D1"] == "U1"
    assert wired_to["R1"] == "J1"
    # A chain is carried by the part at its end, and listed from there outward.
    chain = [sat.ref for sat, _, _ in satellites[parts["U1"]]]
    assert chain.index("D1") < chain.index("R5")


def test_satellites_stay_clear_of_their_host_and_each_other(esp32):
    parts, nets = _sheet(esp32, "usb_charger1")
    satellites = find_satellites(list(parts.values()), nets)
    for host, sats in satellites.items():
        attach_satellites(host, sats, nets)
    release_satellites(satellites)
    cluster = [parts["U1"]] + [sat for sat, _, _ in satellites[parts["U1"]]]
    boxes = [part.lbl_bbox * part.tx for part in cluster]
    for i, box in enumerate(boxes):
        assert not any(box.intersects(other) for other in boxes[i + 1 :])


def test_spread_leaves_no_overlaps():
    """The seed stacks a feedback resistor on its opamp; spreading moves it off."""
    circuit = load_netlist(_doc("opamp_circ.json"))
    preprocess_circuit(circuit)
    nets = [net for net in circuit.nets if len(net.pins) > 1]
    parts = [unit for part in circuit.parts for unit in (part.unit.values() or [part])]
    add_placement_bboxes(parts)
    add_anchor_pull_pins(parts, nets)
    seed_placement(parts, nets)
    spread_parts(parts, nets)
    boxes = [part.place_bbox * part.tx for part in parts]
    for i, box in enumerate(boxes):
        assert not any(box.intersects(other) for other in boxes[i + 1 :])


@pytest.mark.parametrize("seed", [1, 2, 3])
def test_board_routes_without_auto_stub(esp32, tmp_path, seed):
    """Every sheet gets wired, with no part on top of another."""
    score = _render(esp32, tmp_path, seed=seed)
    assert score["wirelength"] > 0
    assert score["part_overlaps"] == 0


def test_preplace_needs_fewer_bends_than_force_placement(tmp_path):
    doc = _doc("span_hier.json")

    def bends(name, **options):
        return sum(
            _render(doc, tmp_path / f"{name}{seed}", seed=seed, **options)["bends"]
            for seed in (1, 2, 3)
        )

    assert bends("on") < bends("off", preplace=False)


def test_preplace_is_reproducible(esp32, tmp_path):
    def files(path):
        # The ERC report carries a timestamp, so leave it out.
        names = sorted(n for n in os.listdir(path) if not n.endswith("-erc.rpt"))
        return {name: open(os.path.join(path, name)).read() for name in names}

    _render(esp32, tmp_path / "a", seed=1, auto_stub=True)
    _render(esp32, tmp_path / "b", seed=1, auto_stub=True)
    assert files(tmp_path / "a") == files(tmp_path / "b")
