# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""Command-line interface: generic netlist JSON -> schematic files."""

import argparse
import sys

from . import __version__
from .render import SUPPORTED_FORMATS, SUPPORTED_TOOLS, render


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
        "--flatness",
        type=float,
        default=0.0,
        help="Hierarchy flattening, 0.0 (fully hierarchical, one file per "
        "sheet) .. 1.0 (everything on one sheet).",
    )
    parser.add_argument(
        "--version",
        action="version",
        version=f"%(prog)s {__version__}",
    )
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    try:
        out = render(
            args.netlist,
            tool=args.tool,
            filepath=args.output,
            top_name=args.top_name,
            title=args.title,
            format=args.format,
            flatness=args.flatness,
        )
    except (ValueError, TypeError, FileNotFoundError) as e:
        print(f"schematizer: error: {e}", file=sys.stderr)
        return 1
    print(f"Wrote {args.format} files to {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
