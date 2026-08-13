# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""
Render a generic netlist document into KiCad schematic files.

``render`` is the single public entry point. A front end (SKiDL, or any tool
that emits the netlist format) calls it in-process, and the CLI wraps it. It
depends only on the vendored engine in :mod:`schematizer.engine` — no skidl.
"""

import json
import os

from .loader import load_netlist

# KiCad versions this tool can target. The engine is one modern writer plus a
# small KiCad-8 format toggle; every version routes through it.
#
# KiCad 5 is deliberately absent. It needs the legacy EESCHEMA ``.sch`` format,
# not the s-expression ``.kicad_sch`` this writer emits, and its libraries
# describe symbols with a different (non-s-expression) draw representation that
# the generic netlist has no slot for. Supporting it would mean a second writer
# and a second symbol format, so it is rejected up front instead.
SUPPORTED_TOOLS = (
    "kicad6",
    "kicad7",
    "kicad8",
    "kicad9",
    "kicad10",
)

# Output formats. Placement and routing are identical for both -- only the
# final serialization differs, so an SVG page has the same layout as the
# .kicad_sch it would have produced.
#
#   "kicad" -- editable .kicad_sch files, one per hierarchical sheet.
#   "svg"   -- static .svg pages, one per hierarchical sheet, cross-linked:
#              a child sheet appears in its parent as a hyperlinked rectangle,
#              and each child page links back to its parent and to the top.
SUPPORTED_FORMATS = ("kicad", "svg")

# Rejected with a specific explanation rather than the generic "unsupported".
_RETIRED_TOOLS = {
    "kicad5": (
        "KiCad 5 uses the legacy EESCHEMA '.sch' format, which this tool does "
        "not write. Use KiCad 6 or later for schematic generation."
    ),
}


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
    format="kicad",
    seed=None,
    **options,
):
    """Generate schematic files from a generic netlist document.

    Args:
        netlist (dict | str | os.PathLike): The generic netlist document, or a
            path to a JSON file containing it.
        tool (str): Target KiCad version, ``"kicad6"`` … ``"kicad10"``.
            KiCad 5 is not supported; see :data:`SUPPORTED_TOOLS`. The version
            still matters for ``format="svg"`` because the embedded symbol
            graphics are KiCad-version flavored.
        filepath (str): Output directory for the generated files.
        top_name (str, optional): Base name for the output files. Defaults to
            the document's ``top_name``, then ``"schematic"``.
        title (str): Schematic title block text.
        format (str): ``"kicad"`` for editable ``.kicad_sch`` files (default),
            or ``"svg"`` for static, cross-linked ``.svg`` pages. See
            :data:`SUPPORTED_FORMATS`.
        seed (optional): Seed for the placer/router's random number generator.
            Placement starts from random positions, so the same netlist
            normally yields a different (equally valid) drawing each run.
            Passing a seed makes a run reproducible: the same netlist, seed and
            options produce byte-identical output. ``None`` (default) keeps the
            existing random behavior. Useful for regression-testing a drawing,
            for filing a bug against a specific bad layout, and for re-rolling
            a layout you don't like by trying successive seeds.
        **options: Passed through to the placement/routing/writer engine
            (e.g. ``flatness``, ``retries``, ``auto_stub``). ``flatness``
            controls hierarchy for both formats: 0 keeps every subcircuit on
            its own sheet, 1 collapses everything onto one.

    Returns:
        str: The output directory (``filepath``).
    """
    if tool in _RETIRED_TOOLS:
        raise ValueError(_RETIRED_TOOLS[tool])
    if tool not in SUPPORTED_TOOLS:
        raise ValueError(
            f"Unsupported tool {tool!r}; choose one of {', '.join(SUPPORTED_TOOLS)}."
        )
    if format not in SUPPORTED_FORMATS:
        raise ValueError(
            f"Unsupported format {format!r}; "
            f"choose one of {', '.join(SUPPORTED_FORMATS)}."
        )

    doc = _load_document(netlist)
    top_name = top_name or doc.get("top_name") or "schematic"
    title = title or doc.get("title") or "SKiDL-Generated Schematic"

    os.makedirs(filepath, exist_ok=True)

    # Reconstruct the IR circuit from the document.
    circuit = load_netlist(doc, tool=tool)

    # Point the engine's per-version output toggles at the requested tool, then
    # drive placement/routing/writing.
    from .engine import gen_schematic as gen_schematic_mod
    from .engine import sexp_schematic as sexp_mod

    gen_schematic_mod.TARGET_TOOL = tool
    sexp_mod.TARGET_TOOL = tool

    gen_schematic_mod.gen_schematic(
        circuit,
        filepath=filepath,
        top_name=top_name,
        title=title,
        output_format=format,
        seed=seed,
        **options,
    )

    return filepath
