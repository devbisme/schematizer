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
import random
import shutil
import tempfile

from .loader import load_netlist
from .quality import score_files

# Wire length (mm) a bend or an overlap is worth when comparing layouts for ``tries``.
LAYOUT_BEND_COST = 5
LAYOUT_OVERLAP_COST = 10

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

# Layout modes.
#
#   "normal" -- placed and routed drawing meant for people to read.
#   "labels" -- labels-only: parts on a plain grid, every pin stubbed to a power
#               symbol, label or no-connect flag, no wires between parts.
#               Connectivity is exact by construction; KiCad output only.
SUPPORTED_MODES = ("normal", "labels")

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
    mode="normal",
    seed=None,
    tries=1,
    progress=None,
    check_netlist=False,
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
        mode (str): ``"normal"`` (placed and routed, default) or ``"labels"``
            (labels-only: exact connectivity, no routing; KiCad format only).
            See :data:`SUPPORTED_MODES`.
        seed (optional): Seed for the placer/router's random number generator.
            Placement starts from random positions, so the same netlist
            normally yields a different (equally valid) drawing each run.
            Passing a seed makes a run reproducible: the same netlist, seed and
            options produce byte-identical output. ``None`` (default) keeps the
            existing random behavior. Useful for regression-testing a drawing,
            for filing a bug against a specific bad layout, and for re-rolling
            a layout you don't like by trying successive seeds.
        tries (int): Number of layouts to generate, keeping the one with the
            lowest score (see :func:`layout_cost`) from
            :mod:`schematizer.quality`. Takes about ``tries`` times as long.
            The seed still makes the result reproducible. Ignored when
            ``mode="labels"``, which has no layout to vary.
        progress: ``True`` shows the step being worked on ("routing fpga") as a
            status line on stderr. A function is called with each message
            instead. Off by default.
        check_netlist (bool): After writing, have KiCad export the netlist of
            the schematic and compare it with ``netlist``. Raises
            :class:`~schematizer.netcheck.NetlistMismatch` if they differ; the
            files are left in place. Needs ``kicad-cli``, and ``format="kicad"``.
            With ``tries``, only the layout that's kept is checked.
        **options: Passed through to the placement/routing/writer engine
            (e.g. ``flatness``, ``retries``, ``auto_stub``). ``flatness``
            controls hierarchy for both formats: 0 keeps every subcircuit on
            its own sheet, 1 collapses everything onto one.
            ``preplace=False`` goes back to force-directed placement from
            random starting positions (see :mod:`schematizer.engine.preplace`).
            ``power_symbols=False`` wires power nets instead of putting a
            power symbol on each of their pins. ``nudge=False`` leaves parts
            where placement put them instead of moving them to straighten
            routed wires (see :mod:`schematizer.engine.nudge`). ``auto_stub=True`` draws nets that
            are hard to route with labels; an integer N does too, with nets of
            more than N pins counted as hard (True means 3). ``label_local_nets=True``
            also labels the named nets that stay on one sheet.

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

    if mode not in SUPPORTED_MODES:
        raise ValueError(
            f"Unsupported mode {mode!r}; choose one of {', '.join(SUPPORTED_MODES)}."
        )
    if mode == "labels" and format != "kicad":
        raise ValueError("Labels-only mode writes KiCad files only (format='kicad').")

    if tries < 1:
        raise ValueError(f"tries must be at least 1, not {tries}.")
    if not (progress is None or isinstance(progress, bool) or callable(progress)):
        raise ValueError("progress must be True, False or a function taking a message.")

    if check_netlist and format != "kicad":
        raise ValueError("check_netlist needs KiCad files (format='kicad').")

    from .engine.progress import report, reporting

    with reporting(progress):
        doc = _load_document(netlist)
        out = _render(doc, tool, filepath, top_name, title, format, mode, seed, tries, options)
        if check_netlist:
            from .netcheck import check_netlist as check

            report("checking the netlist")
            name = top_name or doc.get("top_name") or "schematic"
            check(doc, os.path.join(out, f"{name}.kicad_sch"))
        return out


def _render(doc, tool, filepath, top_name, title, format, mode, seed, tries, options):
    """Do the work of ``render()`` once its arguments are checked."""
    if tries > 1 and mode == "normal":
        kwargs = dict(tool=tool, top_name=top_name, title=title, format=format)
        return _render_best_layout(doc, filepath, seed, tries, kwargs, options)

    top_name = top_name or doc.get("top_name") or "schematic"
    title = title or doc.get("title") or "SKiDL-Generated Schematic"

    os.makedirs(filepath, exist_ok=True)

    # Reconstruct the IR circuit from the document.
    circuit = load_netlist(doc, tool=tool)

    # Point the engine's per-version output toggles at the requested tool, then
    # drive placement/routing/writing.
    from .engine import gen_schematic as gen_schematic_mod
    from .engine import sexp_schematic as sexp_mod
    from .engine.progress import report

    gen_schematic_mod.TARGET_TOOL = tool
    sexp_mod.TARGET_TOOL = tool

    if mode == "labels":
        from .engine.labels import write_labels_schematic

        report("writing labels-only schematic")
        write_labels_schematic(circuit, filepath, top_name, title)
        return filepath

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


def layout_cost(score):
    """Combine the scores of a layout from ``score_files()`` into one number."""
    overlaps = score["part_overlaps"] + score["text_overlaps"]
    return (
        score["wirelength"]
        + LAYOUT_BEND_COST * score["bends"]
        + LAYOUT_OVERLAP_COST * overlaps
    )


def _render_best_layout(doc, filepath, seed, tries, kwargs, options):
    """Render several layouts and keep the files of the one with the best score."""
    from .engine.place import PlacementFailure
    from .engine.progress import prefixed
    from .engine.route import RoutingFailure

    if seed is None:
        seed = random.getrandbits(64)

    # A labels-only fallback has no wires, so it would always score best. Let
    # such a layout fail here; it's the last resort if no layout can be routed.
    fallback = options.get("auto_stub_fallback", "labels")
    trial_options = {**options, "auto_stub_fallback": "raise"}

    # Overlaps can only be scored from KiCad files, so an SVG is drawn from the
    # seed of the best layout once that's known.
    trial_kwargs = {**kwargs, "format": "kicad"}

    work_dir = tempfile.mkdtemp()
    try:
        best_dir, best_seed, best_cost = None, None, float("inf")
        for i in range(tries):
            trial_dir = os.path.join(work_dir, str(i))
            trial_seed = f"{seed}:{i}"
            try:
                with prefixed(f"layout {i + 1}/{tries},"):
                    render(doc, filepath=trial_dir, seed=trial_seed, **trial_kwargs, **trial_options)
            except (PlacementFailure, RoutingFailure):
                continue
            cost = layout_cost(score_files(trial_dir))
            if cost < best_cost:
                best_dir, best_seed, best_cost = trial_dir, trial_seed, cost

        if best_dir is None:
            # Every layout failed, so handle that as the caller asked.
            final_options = {**options, "auto_stub_fallback": fallback}
            return render(doc, filepath=filepath, seed=seed, **kwargs, **final_options)

        if kwargs["format"] != "kicad":
            return render(doc, filepath=filepath, seed=best_seed, **kwargs, **trial_options)

        os.makedirs(filepath, exist_ok=True)
        shutil.copytree(best_dir, filepath, dirs_exist_ok=True)
        return filepath
    finally:
        shutil.rmtree(work_dir, ignore_errors=True)
