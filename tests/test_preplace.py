# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""Tests for pre-placement: satellites, the seed, and spreading the seeded parts."""

import json
import os

import pytest

from schematizer import render
from schematizer.engine import gen_schematic, preplace, tool_module
from schematizer.engine.gen_schematic import (
    preprocess_circuit,
    stub_lone_pins,
    stub_power_nets,
)
from schematizer.engine.net_terminal import NetTerminal
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
    satellites = {
        host: attach_satellites(host, sats, nets) for host, sats in satellites.items()
    }
    release_satellites(satellites)
    cluster = [parts["U1"]] + [sat for sat, _, _ in satellites[parts["U1"]]]
    boxes = [part.lbl_bbox * part.tx for part in cluster]
    for i, box in enumerate(boxes):
        assert not any(box.intersects(other) for other in boxes[i + 1 :])


def test_satellite_with_no_room_is_left_to_be_placed_normally(esp32, monkeypatch):
    """A satellite is never put on top of something for want of a free spot."""
    # Nothing fits within one grid step of a pin.
    preplace._spot_offsets.cache_clear()
    monkeypatch.setattr(preplace, "SAT_MAX_OUT", 1)
    monkeypatch.setattr(preplace, "SAT_MAX_SIDE", 1)
    try:
        parts, nets = _sheet(esp32, "usb_charger1")
        u1 = parts["U1"]
        satellites = find_satellites(list(parts.values()), nets)
        assert satellites[u1]
        assert attach_satellites(u1, satellites[u1], nets) == []
        assert u1.riders == []
    finally:
        preplace._spot_offsets.cache_clear()


def test_spread_finds_room_however_crowded(monkeypatch):
    """Parts seeded in a pile are all moved apart, even past the usual search window."""
    monkeypatch.setattr(preplace, "SPREAD_MAX_STEPS", 1)
    circuit = load_netlist(_doc("span_hier.json"))
    preprocess_circuit(circuit)
    parts = list(circuit.parts)
    add_placement_bboxes(parts)
    spread_parts(parts, [])  # No nets, and every part starts at the origin.
    boxes = [part.place_bbox * part.tx for part in parts]
    for i, box in enumerate(boxes):
        assert not any(box.intersects(other) for other in boxes[i + 1 :])


def _placed_tree(doc, **options):
    """The tree of nodes for a netlist, with its parts and net labels placed."""
    circuit = load_netlist(doc)
    stub_power_nets(circuit, keep_drawn=True)
    options = dict(use_push_pull=True, rotate_parts=True, pt_to_pt_mult=5, pin_normalize=True, **options)
    node = gen_schematic._new_node(circuit, tool_module, ".", "sch", "", 0.0, options)
    node.place(**options)
    return circuit, node


def _nodes(node):
    yield node
    for child in node.children.values():
        yield from _nodes(child)


def test_lone_pin_of_a_net_on_a_sheet_gets_a_label(esp32):
    """A net's only pin on a sheet is labeled there, so nothing needs routing to it."""
    circuit = load_netlist(esp32)
    stub_lone_pins(circuit)
    pins = {(pin.part.ref, str(pin.num)): pin for part in circuit.parts for pin in part.pins}
    # I2S_BCLK goes from the MCU's sheet to the amplifier's.
    assert pins["U3", "7"].stub and pins["U4", "16"].stub
    # USB_DM has two pins on the connector's sheet, which get wired, and one on the MCU's.
    assert not pins["J1", "A7"].stub and not pins["J1", "B7"].stub
    assert pins["U3", "13"].stub
    # A net that stays on its sheet is wired.
    assert not pins["D1", "1"].stub


def test_net_labels_are_only_for_nets_that_leave_the_sheet():
    doc = _doc("and_gate.json")  # One sheet, with the named net A_AND_B wired on it.

    def terminals(**options):
        _, node = _placed_tree(doc, **options)
        return [p.pins[0].net.name for n in _nodes(node) for p in n.parts if isinstance(p, NetTerminal)]

    assert terminals() == []
    assert "A_AND_B" in terminals(label_local_nets=True)


def test_net_labels_sit_beside_pins_clear_of_everything(esp32):
    _, node = _placed_tree(esp32)
    labels = 0
    for sheet in _nodes(node):
        boxes = {part: part.lbl_bbox * part.tx for part in sheet.parts}
        for part, box in boxes.items():
            if isinstance(part, NetTerminal):
                labels += 1
                assert not any(
                    box.intersects(other) for p, other in boxes.items() if p is not part
                )
    assert labels  # USB_DM and USB_DP on the connector's sheet.


def test_sheet_symbol_is_tall_enough_for_its_pins(esp32):
    """Sheet pins are a grid apart, and none can hang below the sheet's box."""
    _, node = _placed_tree(esp32)
    for sheet in node.children.values():
        num_pins = sum(not net.stub for net in sheet.get_boundary_nets())
        assert sheet.external_bbox().h >= (num_pins + 1) * 100


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


def test_auto_stub_keeps_preplaced_parts_wired_and_apart(esp32, tmp_path):
    """Auto-stubbing doesn't snap pre-placed parts onto pins, so none overlap."""
    score = _render(esp32, tmp_path, seed=1, auto_stub=True)
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
