# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""Command-line interface: generic netlist JSON -> schematic files."""

import argparse
import sys

from . import __version__
from .engine.gen_schematic import AUTO_STUB_PINS
from .engine.place import PlacementFailure
from .engine.route import RoutingFailure
from .netcheck import NetlistMismatch
from .render import SUPPORTED_FORMATS, SUPPORTED_MODES, SUPPORTED_TOOLS, render


def build_parser():
    parser = argparse.ArgumentParser(
        prog="schematizer",
        description="Generate KiCad schematic or SVG files from a generic "
        "hierarchical netlist (JSON).",
    )
    parser.add_argument(
        "netlist",
        help="Path to the generic netlist JSON file.",
    )
    parser.add_argument(
        "-t",
        "--tool",
        default="kicad9",
        choices=SUPPORTED_TOOLS,
        help="Target KiCad version (default: kicad9).",
    )
    parser.add_argument(
        "-f",
        "--format",
        default="kicad",
        choices=SUPPORTED_FORMATS,
        help="Output format: editable .kicad_sch files, or static "
        "cross-linked .svg pages (default: kicad).",
    )
    parser.add_argument(
        "-o",
        "--output",
        default=".",
        help="Output directory for the generated files (default: .).",
    )
    parser.add_argument(
        "--top-name",
        default=None,
        help="Base name for output files (default: from the netlist).",
    )
    parser.add_argument(
        "--title",
        default="SKiDL-Generated Schematic",
        help="Schematic title-block text.",
    )
    parser.add_argument(
        "-m",
        "--mode",
        default="normal",
        choices=SUPPORTED_MODES,
        help="Layout mode: 'normal' (placed and routed) or 'labels' "
        "(labels-only, exact connectivity, no routing) (default: normal).",
    )
    parser.add_argument(
        "--flatness",
        type=float,
        default=0.0,
        help="Hierarchy flattening, 0.0 (fully hierarchical, one file per "
        "sheet) .. 1.0 (everything on one sheet).",
    )
    parser.add_argument(
        "--seed",
        type=int,
        default=None,
        help="Seed the placer/router so the drawing is reproducible: the same "
        "netlist, seed and options give byte-identical output. Omit for a "
        "different (equally valid) layout each run.",
    )
    parser.add_argument(
        "--tries",
        type=int,
        default=1,
        help="Generate this many layouts and keep the one with the best wiring "
        "score. Takes about this many times as long.",
    )
    parser.add_argument(
        "--sheet-tries",
        type=int,
        default=1,
        metavar="N",
        help="Lay out each sheet this many times and keep its best layout. "
        "Unlike --tries, each sheet gets the best of its own tries.",
    )
    parser.add_argument(
        "--no-reuse",
        action="store_true",
        help="Lay out every sheet separately. Normally a sheet holding the same "
        "circuit as an earlier one is drawn the same way.",
    )
    parser.add_argument(
        "--no-preplace",
        action="store_true",
        help="Place connected parts with force-directed placement from random "
        "starting positions, instead of putting each part beside the pins it's "
        "wired to.",
    )
    parser.add_argument(
        "--no-power-symbols",
        action="store_true",
        help="Wire power nets (GND, VCC, +3V3, ...) like any other net instead "
        "of putting a power symbol on each of their pins.",
    )
    parser.add_argument(
        "--no-nudge",
        action="store_true",
        help="Leave parts where placement put them. Normally, once a sheet is "
        "routed, parts are moved a step or two to straighten wires that jog "
        "sideways and the sheet is routed again.",
    )
    parser.add_argument(
        "--auto-stub",
        nargs="?",
        type=int,
        const=AUTO_STUB_PINS,
        default=False,
        metavar="PINS",
        help="Draw hard-to-route nets with labels instead of wires: nets with "
        f"more than PINS pins (default: {AUTO_STUB_PINS}) and nets whose pins are far "
        "apart. Fixes what KiCad's ERC still finds (needs kicad-cli), and writes "
        "a labels-only schematic if routing still fails.",
    )
    parser.add_argument(
        "--check-netlist",
        action="store_true",
        help="After writing, have KiCad export the schematic's netlist and "
        "compare it with the input. Exits with status 1 if they differ "
        "(needs kicad-cli; KiCad format only).",
    )
    parser.add_argument(
        "--progress",
        dest="progress",
        action="store_true",
        default=None,
        help="Show which sheet is being placed or routed (default: on when "
        "stderr is a terminal).",
    )
    parser.add_argument(
        "--no-progress",
        dest="progress",
        action="store_false",
        help="Don't show progress.",
    )
    parser.add_argument(
        "--version",
        action="version",
        version=f"%(prog)s {__version__}",
    )
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    progress = sys.stderr.isatty() if args.progress is None else args.progress
    try:
        out = render(
            args.netlist,
            tool=args.tool,
            filepath=args.output,
            top_name=args.top_name,
            title=args.title,
            format=args.format,
            mode=args.mode,
            flatness=args.flatness,
            seed=args.seed,
            tries=args.tries,
            sheet_tries=args.sheet_tries,
            reuse_layouts=not args.no_reuse,
            preplace=not args.no_preplace,
            power_symbols=not args.no_power_symbols,
            nudge=not args.no_nudge,
            auto_stub=args.auto_stub,
            progress=progress,
            check_netlist=args.check_netlist,
        )
    except (ValueError, TypeError, FileNotFoundError, RuntimeError, NetlistMismatch) as e:
        print(f"schematizer: error: {e}", file=sys.stderr)
        return 1
    except (PlacementFailure, RoutingFailure) as e:
        stage = "placed" if isinstance(e, PlacementFailure) else "routed"
        detail = f" ({e})" if str(e) else ""
        print(
            f"schematizer: error: the drawing couldn't be {stage}{detail}. "
            "Try --auto-stub, more --tries, another --seed, or --mode labels.",
            file=sys.stderr,
        )
        return 1
    print(f"Wrote {args.format} files to {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
