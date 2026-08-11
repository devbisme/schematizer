# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""
The vendored engine is a single KiCad backend, so it *is* its own "tool
module". SKiDL passed a per-version module object around (``tool_module``) that
the placement/routing code queried for constants and bounding-box helpers; this
module reproduces that small surface (``constants``, ``calc_hier_label_bbox``,
``calc_symbol_bbox``, ``write_top_schematic``) so ``SchNode`` and
``NetTerminal`` can use it unchanged.
"""

from . import constants
from .bboxes import calc_hier_label_bbox, calc_symbol_bbox
from .sexp_schematic import write_top_schematic

__all__ = [
    "constants",
    "calc_hier_label_bbox",
    "calc_symbol_bbox",
    "write_top_schematic",
]
