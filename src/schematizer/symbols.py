# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""
Rebuild KiCad symbol libraries from the *embedded* symbol definitions in a
generic netlist document, so parts can be reconstructed without any access to
the original KiCad symbol libraries.

Each unique ``lib_id`` in the document carries the raw ``draw_cmds`` (the KiCad
symbol graphics + pins) that SKiDL parsed from the source library. Here we
serialize those back into a temporary ``.kicad_sym`` file and load it, which
exercises the real SKiDL library parser on the embedded data.

Scaffold note: this round-trips embedded symbols through a temporary library
because the package currently reuses SKiDL's Part/Pin object model. The future
IR-based loader will feed the embedded ``draw_cmds`` straight to the writer with
no re-parse (see the common-graphics caveat below).
"""

import os
import tempfile

from simp_sexp import Sexp


def _symbol_to_sexp(part_name, sym):
    """Rebuild a ``(symbol ...)`` s-expr for one embedded symbol entry."""
    draw = sym.get("draw_cmds", {})
    s = [
        "symbol",
        part_name,
        ["pin_numbers", ["hide", "yes"]],
        ["pin_names", ["offset", 0]],
        ["exclude_from_sim", "no"],
        ["in_bom", "yes"],
        ["on_board", "yes"],
        [
            "property",
            "Reference",
            sym.get("ref_prefix", "U"),
            ["at", 2.032, 0, 90],
            ["effects", ["font", ["size", 1.27, 1.27]]],
        ],
        [
            "property",
            "Value",
            part_name,
            ["at", 0, 0, 90],
            ["effects", ["font", ["size", 1.27, 1.27]]],
        ],
        [
            "property",
            "Footprint",
            "",
            ["at", 0, 0, 0],
            ["effects", ["font", ["size", 1.27, 1.27]], ["hide", "yes"]],
        ],
        [
            "property",
            "Datasheet",
            sym.get("datasheet") or "~",
            ["at", 0, 0, 0],
            ["effects", ["font", ["size", 1.27, 1.27]], ["hide", "yes"]],
        ],
    ]
    if sym.get("description"):
        s.append(
            [
                "property",
                "Description",
                sym["description"],
                ["at", 0, 0, 0],
                ["effects", ["font", ["size", 1.27, 1.27]], ["hide", "yes"]],
            ]
        )

    # Common graphics live in the "0" (all-units) sub-symbol.
    have_common = "0" in draw and any(
        c and c[0] != "pin" for c in draw["0"]
    )
    if "0" in draw:
        graphics = [c for c in draw["0"] if c and c[0] != "pin"]
        if graphics:
            s.append(["symbol", f"{part_name}_0_1"] + graphics)

    # Per-unit sub-symbols carry pins (and any genuinely unit-specific
    # graphics). SKiDL's flattened draw_cmds copy the common _0_1 graphics into
    # every unit; the library parser re-injects them on load, so we must NOT
    # re-emit them here or the symbol body is drawn twice.
    for unit_num, cmds in draw.items():
        if unit_num == "0":
            continue
        pins = [c for c in cmds if c and c[0] == "pin"]
        graphics = [c for c in cmds if c and c[0] not in ("pin", "property")]
        if have_common:
            graphics = []
        if pins or graphics:
            s.append(
                ["symbol", f"{part_name}_{unit_num}_{unit_num}"]
                + graphics
                + pins
            )
    return s


def build_symbol_libs(doc, tool, tmpdir=None):
    """Write one temp ``.kicad_sym`` per library and load each with SKiDL.

    Args:
        doc (dict): Generic netlist document with a ``symbols`` section.
        tool (str): KiCad tool name (e.g. ``"kicad9"``) for the SKiDL parser.
        tmpdir (str, optional): Directory for the temp libraries; created if
            not given.

    Returns:
        dict: ``{lib_name: SchLib}``, with each SchLib's ``filename`` set to the
        bare library name so generated ``lib_id``s read as ``"Device:R"``.
    """
    from skidl import SchLib

    if tmpdir is None:
        tmpdir = tempfile.mkdtemp(prefix="schematizer_embed_")

    # Group embedded symbols by their originating library name.
    by_lib = {}
    for lib_id, sym in doc.get("symbols", {}).items():
        lib_name, part_name = lib_id.split(":", 1)
        by_lib.setdefault(lib_name, {})[part_name] = sym

    libs = {}
    for lib_name, syms in by_lib.items():
        lib_sexp = Sexp(
            ["kicad_symbol_lib", ["version", 20211014], ["generator", "skidl"]]
        )
        for part_name, sym in syms.items():
            lib_sexp.append(Sexp(_symbol_to_sexp(part_name, sym)))
        lib_sexp.add_quotes(lambda s: True)

        path = os.path.join(tmpdir, f"{lib_name}.kicad_sym")
        with open(path, "w") as f:
            f.write(lib_sexp.to_str())

        lib = SchLib(path, tool=tool, use_cache=False, use_pickle=False)
        # Present the bare library name (not the temp path) so lib_ids match.
        lib.filename = lib_name
        libs[lib_name] = lib

    return libs
