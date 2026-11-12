# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""Parts are only turned in ways that follow the drafting conventions:
rails up, grounds down, signals left to right, ICs upright."""

import json
import os

import pytest

from schematizer.engine.conventions import (
    is_power_net,
    DOWN,
    UP,
    best_orientations,
    orientation_cost,
    pin_exit,
    rail_dir,
)
from schematizer.engine.gen_schematic import preprocess_circuit, stub_power_nets
from schematizer.geometry import Tx
from schematizer.loader import load_netlist

DATA = os.path.join(os.path.dirname(__file__), os.pardir, "data")


def _circuit(name):
    with open(os.path.join(DATA, name)) as f:
        circuit = load_netlist(json.load(f))
    preprocess_circuit(circuit)
    return circuit


@pytest.fixture(scope="module")
def parts():
    return {part.ref: part for part in _circuit("esp32_audio_board.json").parts}


class _Net:
    def __init__(self, name):
        self.name = name


@pytest.mark.parametrize(
    "name,direction",
    [
        ("VCC", UP),
        ("+3V3", UP),
        ("VBAT", UP),
        ("GND", DOWN),
        ("AGND", DOWN),
        ("VSS", DOWN),
        ("-12V", DOWN),
        ("SDA", None),
        ("N$1", None),
    ],
)
def test_rail_dir(name, direction):
    assert rail_dir(_Net(name)) == direction


def _exit(part, net_name):
    pin = next(pin for pin in part.pins if pin.net and pin.net.name == net_name)
    exit_vec = pin_exit(pin, part.tx)
    return (round(exit_vec.x), round(exit_vec.y))


def test_decoupling_cap_is_upright(parts):
    """A rail that isn't named "+..." still goes on top."""
    assert _exit(parts["C6"], "VCC") == UP
    assert _exit(parts["C6"], "GND") == DOWN
    assert orientation_cost(parts["C6"]) == 0


def test_rail_part_can_only_be_mirrored(parts):
    """Of a cap's eight orientations, only upright and its mirror image are allowed."""
    allowed = best_orientations(parts["C6"])
    assert len(allowed) == 2
    assert all(_exit_with(parts["C6"], tx, "GND") == DOWN for tx in allowed)


def _exit_with(part, tx, net_name):
    pin = next(pin for pin in part.pins if pin.net and pin.net.name == net_name)
    exit_vec = pin_exit(pin, tx)
    return (round(exit_vec.x), round(exit_vec.y))


def test_ic_is_not_turned_sideways(parts):
    """The regulator's input rail is on its left, but it isn't rotated to put that on top."""
    u2 = parts["U2"]
    assert (u2.tx.b, u2.tx.c) == (0, 0)
    assert all((tx.b, tx.c) == (0, 0) for tx in best_orientations(u2))
    assert orientation_cost(u2, Tx().rot_90cw()) > orientation_cost(u2)


def test_signal_flow_keeps_inputs_on_the_left(parts):
    """Mirroring the amplifier would make its inputs face right, so that's not allowed."""
    u4 = parts["U4"]
    assert orientation_cost(u4, Tx().flip_x()) > orientation_cost(u4)


def test_power_nets_become_power_symbols_unless_drawn():
    with open(os.path.join(DATA, "esp32_audio_board.json")) as f:
        circuit = load_netlist(json.load(f))
    assert "GND" in {net.name for net in stub_power_nets(circuit, keep_drawn=True)}

    # and_gate.json has its own GND and VCC power symbol parts wired in.
    with open(os.path.join(DATA, "and_gate.json")) as f:
        circuit = load_netlist(json.load(f))
    assert stub_power_nets(circuit, keep_drawn=True) == []


class _Pin:
    def __init__(self, func, part=None):
        self.func = func
        self.part = part


def _net(name, *funcs):
    net = _Net(name)
    net.pins = [_Pin(func) for func in funcs]
    return net


@pytest.mark.parametrize(
    "net,is_rail",
    [
        # A rail with a name of its own, found by its power pins.
        (_net("VBUS_SW", "POWER-OUT", "POWER-IN", "PASSIVE", "PASSIVE"), True),
        # A regulator's switch node: a power pin, but too few pins to be distributing power.
        (_net("SW_3V3", "POWER-OUT", "PASSIVE"), False),
        # Lots of pins but no power pin.
        (_net("ATT_A", *["PASSIVE"] * 7), False),
        # Named like a rail.
        (_net("+5V", "PASSIVE"), True),
    ],
)
def test_rails_found_by_name_or_power_pins(net, is_rail):
    assert is_power_net(net) == is_rail
    assert (rail_dir(net) == UP) == is_rail


def test_rail_points_the_way_of_its_power_symbol():
    """opamp_circ.json draws ground with a GND symbol on a net named N$2."""
    circuit = _circuit("opamp_circ.json")
    nets = {net.name: net for net in circuit.nets}
    assert rail_dir(nets["N$2"]) == DOWN  # GND symbol.
    assert rail_dir(nets["N$1"]) == UP  # VCC symbol.


def test_power_symbol_parts_keep_their_library_orientation():
    circuit = _circuit("opamp_circ.json")
    for part in circuit.parts:
        if part.is_power_symbol:
            assert (part.tx.a, part.tx.b, part.tx.c, part.tx.d) == (1, 0, 0, 1)
