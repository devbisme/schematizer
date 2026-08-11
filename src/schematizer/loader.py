# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""
Reconstruct an IR circuit from a generic netlist document.

The document describes electrical interconnection, hierarchy, embedded symbol
geometry, and layout hints. This module turns that into a graph of lightweight
:mod:`schematizer.ir` objects (``SchPart``/``SchPin``/``SchNet``) that the
placement/routing/writer engine consumes directly.

No KiCad symbol libraries are read and nothing is re-parsed: the embedded
``draw_cmds`` are handed to the writer as-is, and pin geometry comes straight
from the document. (An earlier scaffold round-tripped symbols through a
temporary ``.kicad_sym`` file and SKiDL's parser; that is gone.)
"""

from simp_sexp import Sexp

from .ir import Circuit, NCNet, SchNet, SchPart, SchPartUnit, SchPin

SCHEMA_NAME = "skidl-generic-netlist"
SCHEMA_VERSION = 1


def _check_document(doc):
    """Validate the document is a netlist of a supported schema version."""
    fmt = doc.get("format")
    if fmt != SCHEMA_NAME:
        raise ValueError(
            f"Unrecognized netlist format {fmt!r}; expected {SCHEMA_NAME!r}."
        )
    if doc.get("version") != SCHEMA_VERSION:
        raise ValueError(
            f"Unsupported netlist schema version {doc.get('version')!r}; "
            f"this tool understands version {SCHEMA_VERSION}."
        )


def _draw_cmds_to_ir(raw):
    """Convert JSON draw_cmds ({str unit: [cmd, ...]}) to the engine's form.

    The writer indexes ``draw_cmds`` with integer unit numbers and treats each
    command as an s-expression, so keys are cast to ``int`` and commands wrapped
    back into ``Sexp`` (they were flattened to plain lists for JSON transport).
    """
    out = {}
    for unit_num, cmds in (raw or {}).items():
        out[int(unit_num)] = [Sexp(cmd) for cmd in cmds]
    return out


def _make_part(comp, symbols, nets, circuit):
    """Instantiate one component (with pins connected) into the circuit."""
    lib_id = comp["lib_id"]
    sym = symbols.get(lib_id, {})

    part = SchPart(
        name=sym.get("name") or lib_id.split(":", 1)[-1],
        ref=comp["ref"],
        ref_prefix=sym.get("ref_prefix", "U"),
        value=comp.get("value", "") or "",
        footprint=comp.get("footprint", "") or "",
        lib_id=lib_id,
        description=sym.get("description", "") or "",
        datasheet=sym.get("datasheet", "") or "",
        circuit=circuit,
    )
    part.hiertuple = tuple(comp.get("hiertuple", ("",)))
    part.symtx = comp.get("symtx", "") or ""
    part.orientation_locked = bool(comp.get("orientation_locked", False))
    part.draw_cmds = _draw_cmds_to_ir(sym.get("draw_cmds"))

    # Which net each pin connects to (keyed by pin number).
    net_of_pin = {p["num"]: p.get("net") for p in comp.get("pins", [])}

    # Pin geometry lives in the embedded symbol, per unit. Build the pins and
    # remember which unit each belongs to.
    pins_by_unit = {}
    for unit_key, unit in sorted(sym.get("units", {}).items(), key=lambda kv: kv[0]):
        for pg in unit.get("pins", []):
            pin = SchPin(
                num=pg["num"],
                name=pg.get("name", "") or "",
                x=pg.get("x", 0),
                y=pg.get("y", 0),
                orientation=pg.get("orient", 0),
            )
            part.add_pins(pin)
            pins_by_unit.setdefault(int(unit_key), []).append(pin)
            net_name = net_of_pin.get(pg["num"])
            if net_name is not None and net_name in nets:
                pin += nets[net_name]

    # A part with more than one unit (e.g. a quad op-amp) is placed as separate
    # units sharing one reference. Single-unit parts keep ``unit`` empty so the
    # node tree places the part itself.
    if len(pins_by_unit) > 1:
        for num in sorted(pins_by_unit):
            label = "u{}".format(chr(ord("A") + num - 1)) if num >= 1 else "u{}".format(num)
            part.unit[label] = SchPartUnit(part, num, label, pins_by_unit[num])

    return part


def load_netlist(doc, tool="kicad9"):
    """Reconstruct an IR circuit from a generic netlist document.

    Args:
        doc (dict): Parsed generic netlist JSON.
        tool (str): Target KiCad tool name (accepted for API symmetry; the
            embedded data is tool-neutral, so it is not needed to build the IR).

    Returns:
        Circuit: An IR circuit ready for schematic generation.
    """
    _check_document(doc)

    circuit = Circuit()
    symbols = doc.get("symbols", {})

    # Create all nets first so pins in any component can attach to them, and
    # apply user net hints (stub/io direction).
    nets = {}
    for n in doc.get("nets", []):
        NetClass = NCNet if n.get("nc") else SchNet
        net = NetClass(name=n["name"], code=n.get("code", 0), circuit=circuit)
        net._implicit = bool(n.get("implicit", False))
        net.netio = (n.get("netio", "") or "").lower()
        nets[n["name"]] = net
        circuit.nets.append(net)

    # Instantiate components and connect their pins.
    for comp in doc.get("components", []):
        _make_part(comp, symbols, nets, circuit)

    # Apply explicit stub hints after pins are connected so the cascade to pins
    # takes effect. (Automatic stubbing is a layout concern handled downstream.)
    for n in doc.get("nets", []):
        if n.get("stub_explicit"):
            net = nets[n["name"]]
            net._stub_explicit = True
            net.stub = bool(n.get("stub", False))

    return circuit
