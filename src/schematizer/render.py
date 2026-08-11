# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""
Render a generic netlist document into KiCad schematic files.

``render`` is the single public entry point. SKiDL's ``generate_schematic``
calls it in-process, and the CLI wraps it.
"""

import json
import os

from .loader import load_netlist

# KiCad versions this tool can target.
SUPPORTED_TOOLS = (
    "kicad5",
    "kicad6",
    "kicad7",
    "kicad8",
    "kicad9",
    "kicad10",
)


def _load_document(netlist):
    """Accept a document dict or a path to a JSON file; return the dict."""
    if isinstance(netlist, dict):
        return netlist
    if isinstance(netlist, (str, bytes, os.PathLike)):
        with open(netlist) as f:
            return json.load(f)
    raise TypeError(
        "netlist must be a document dict or a path to a JSON file, "
        f"got {type(netlist).__name__}."
    )


def render(
    netlist,
    tool="kicad9",
    filepath=".",
    top_name=None,
    title="SKiDL-Generated Schematic",
    **options,
):
    """Generate schematic files from a generic netlist document.

    Args:
        netlist (dict | str | os.PathLike): The generic netlist document, or a
            path to a JSON file containing it.
        tool (str): Target KiCad version, ``"kicad5"`` … ``"kicad10"``.
        filepath (str): Output directory for the ``.kicad_sch`` files.
        top_name (str, optional): Base name for the output files. Defaults to
            the document's ``top_name``, then ``"schematic"``.
        title (str): Schematic title block text.
        **options: Passed through to the placement/routing/writer engine
            (e.g. ``flatness``, ``retries``).

    Returns:
        str: The output directory (``filepath``).
    """
    if tool not in SUPPORTED_TOOLS:
        raise ValueError(
            f"Unsupported tool {tool!r}; choose one of {', '.join(SUPPORTED_TOOLS)}."
        )

    doc = _load_document(netlist)
    top_name = top_name or doc.get("top_name") or "schematic"
    title = title or doc.get("title") or "SKiDL-Generated Schematic"

    os.makedirs(filepath, exist_ok=True)

    # Reconstruct the circuit (merged) from the document.
    circuit = load_netlist(doc, tool=tool)

    # Drive the placement/routing/writer engine directly. This mirrors what
    # SKiDL's generate_schematic does internally (footprint handler + dispatch),
    # but without re-merging (the loader already merged).
    import skidl
    from skidl.tools import tool_modules

    def _empty_footprint_handler(part):
        part.footprint = ":"

    saved_handler = skidl.empty_footprint_handler
    skidl.empty_footprint_handler = _empty_footprint_handler
    try:
        tool_modules[tool].gen_schematic(
            circuit,
            filepath=filepath,
            top_name=top_name,
            title=title,
            **options,
        )
    finally:
        skidl.empty_footprint_handler = saved_handler

    return filepath
