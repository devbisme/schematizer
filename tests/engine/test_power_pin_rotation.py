# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""Parts are pre-rotated so ground pins face down and voltage-rail pins face up,
except for parts whose orientation was set explicitly with symtx."""

import json
import os

import pytest

from schematizer.engine.gen_schematic import preprocess_circuit
from schematizer.geometry import Tx
from schematizer.loader import load_netlist

DATA = os.path.join(os.path.dirname(__file__), os.pardir, "data")


def _circuit(c1_nets, c1_symtx=""):
    """flat.json circuit with C1's pin nets (pin 1 is on top of Device:C) replaced."""
    with open(os.path.join(DATA, "flat.json")) as f:
        doc = json.load(f)
    c1 = next(c for c in doc["components"] if c["ref"] == "C1")
    c1["symtx"] = c1_symtx
    for pin in c1["pins"]:
        pin["net"] = c1_nets[pin["num"]]
    doc["nets"] = [
        {
            "name": name,
            "code": code,
            "stub": False,
            "stub_explicit": False,
            "netio": "",
            "implicit": False,
            "nodes": [
                {"ref": comp["ref"], "pin": p["num"], "pintype": "PASSIVE"}
                for comp in doc["components"]
                for p in comp["pins"]
                if p["net"] == name
            ],
        }
        for code, name in enumerate(
            sorted({p["net"] for c in doc["components"] for p in c["pins"]}), 1
        )
    ]
    ckt = load_netlist(doc, tool="kicad9")
    preprocess_circuit(ckt)
    return next(p for p in ckt.parts if p.ref == "C1")


def _pin_y(part, num):
    """Pin Y in the (Y-up) placement frame after the part's transform."""
    pin = next(p for p in part.pins if str(p.num) == num)
    return (pin.pt * part.tx).y


@pytest.mark.parametrize(
    "c1_nets,top_pin",
    [
        ({"1": "GND", "2": "SIG"}, "2"),  # GND starts on top -> flipped to bottom.
        ({"1": "SIG", "2": "GND"}, "1"),  # GND already on the bottom -> unchanged.
        ({"1": "SIG", "2": "+5V"}, "2"),  # Rail starts on the bottom -> flipped to top.
        ({"1": "+5V", "2": "SIG"}, "1"),  # Rail already on top -> unchanged.
    ],
)
def test_power_pins_oriented(c1_nets, top_pin):
    c1 = _circuit(c1_nets)
    other = "1" if top_pin == "2" else "2"
    assert _pin_y(c1, top_pin) > _pin_y(c1, other)


def test_explicit_symtx_not_rotated():
    """A part rotated/flipped by the user keeps exactly that orientation."""
    c1 = _circuit({"1": "GND", "2": "SIG"}, c1_symtx="H")
    tx, ref = c1.tx, Tx.from_symtx("H")
    assert (tx.a, tx.b, tx.c, tx.d) == (ref.a, ref.b, ref.c, ref.d)
