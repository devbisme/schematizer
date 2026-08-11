# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""
Reconstruct a circuit from a generic netlist document.

The document describes electrical interconnection, hierarchy, embedded symbol
geometry, and layout hints. This module turns that back into an object graph
the placement/routing/writer engine can consume.

Scaffold note: the reconstructed object graph is currently a real SKiDL
``Circuit`` (the package reuses SKiDL's engine). This is where the future
lightweight IR (``SchPart``/``SchPin``/``SchNet``) will slot in, replacing the
SKiDL dependency while keeping the same public ``load_netlist`` contract.
"""

from collections import defaultdict

from .symbols import build_symbol_libs

SCHEMA_NAME = "skidl-generic-netlist"


def _check_document(doc):
    """Validate the document is a netlist of a supported schema version."""
    fmt = doc.get("format")
    if fmt != SCHEMA_NAME:
        raise ValueError(
            f"Unrecognized netlist format {fmt!r}; expected {SCHEMA_NAME!r}."
        )
    if doc.get("version") != 1:
        raise ValueError(
            f"Unsupported netlist schema version {doc.get('version')!r}; "
            f"this tool understands version 1."
        )


def load_netlist(doc, tool="kicad9"):
    """Reconstruct a circuit from a generic netlist document.

    Args:
        doc (dict): Parsed generic netlist JSON.
        tool (str): KiCad tool name used to parse embedded symbols and build
            the circuit (e.g. ``"kicad9"``).

    Returns:
        Circuit: A merged SKiDL circuit ready for schematic generation.
    """
    _check_document(doc)

    from skidl import Circuit, Group, Net, Part, set_default_tool

    set_default_tool(tool)
    ckt = Circuit()

    # Rebuild symbol libraries purely from the embedded definitions.
    libs = build_symbol_libs(doc, tool)

    def make_part(comp, nets):
        lib_name, part_name = comp["lib_id"].split(":", 1)
        part = Part(
            libs[lib_name],
            part_name,
            footprint=comp.get("footprint") or None,
        )
        part.ref = comp["ref"]
        if comp.get("value"):
            part.value = comp["value"]
        if comp.get("symtx"):
            part.symtx = comp["symtx"]
        for pin in comp["pins"]:
            if pin.get("net") is not None:
                part[pin["num"]] += nets[pin["net"]]

    # Group components by hierarchy path and derive the tree of levels so each
    # hierarchy node is created exactly once.
    comps_by_path = defaultdict(list)
    for comp in doc["components"]:
        comps_by_path[tuple(comp["hiertuple"])].append(comp)

    paths = set()
    for p in list(comps_by_path):
        for i in range(1, len(p) + 1):
            paths.add(p[:i])
    children = defaultdict(list)
    for p in sorted(paths):
        if len(p) >= 2:
            parent = p[:-1]
            if p[-1] not in children[parent]:
                children[parent].append(p[-1])

    with ckt:
        # Create all nets up front so pins in different hierarchy groups can
        # share them, and apply user net hints.
        nets = {}
        for n in doc["nets"]:
            net = Net(n["name"])
            nets[n["name"]] = net
            if n.get("stub_explicit"):
                net.stub = n.get("stub", False)
            if n.get("netio"):
                net.netio = n["netio"]

        def build(path):
            for comp in comps_by_path.get(path, []):
                make_part(comp, nets)
            for child_name in children.get(path, []):
                # Force the exact hierarchy name via tag (hiertuple uses
                # tag_or_name) so UUIDs/sheet filenames are reproducible.
                with Group(child_name, tag=child_name):
                    build(path + (child_name,))

        build(("",))

    ckt.merge_net_names()
    ckt.merge_nets()
    return ckt
