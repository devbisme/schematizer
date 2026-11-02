# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""
Score the wiring of generated KiCad schematics.

The score is measured on the written ``.kicad_sch`` files rather than on the
engine's internal state, so it reflects what a user actually sees and works
on any schematic, whatever produced it. Lower is better for every count:

    segments    wire segments drawn
    bends       corners where exactly two perpendicular segments meet
    wirelength  total wire length (mm)

Pins that almost, but don't quite, line up show up here as extra bends. The
placement-time count of those near misses is engine.place.alignment_stats().

Usage::

    python -m schematizer.quality <dir-or-.kicad_sch> ...
"""

import glob
import os
import re
import sys
from collections import defaultdict

__all__ = ["score_schematic", "score_files"]

_WIRE_RE = re.compile(
    r"\(wire\s*\(pts\s*\(xy\s+([-\d.]+)\s+([-\d.]+)\)\s*\(xy\s+([-\d.]+)\s+([-\d.]+)\)"
)


def _wires(sch_text):
    """Extract wire segments as ((x1, y1), (x2, y2)) tuples rounded to 0.01 mm."""
    return [
        ((round(float(x1), 2), round(float(y1), 2)), (round(float(x2), 2), round(float(y2), 2)))
        for x1, y1, x2, y2 in _WIRE_RE.findall(sch_text)
    ]


def score_schematic(sch_text):
    """Score the wiring in the text of one .kicad_sch file.

    Args:
        sch_text (str): Contents of a .kicad_sch file.

    Returns:
        dict: Counts of segments and bends, plus wirelength in mm.
    """
    segs = [s for s in _wires(sch_text) if s[0] != s[1]]

    # Segments meeting at each endpoint.
    ends = defaultdict(list)
    for i, (p1, p2) in enumerate(segs):
        ends[p1].append(i)
        ends[p2].append(i)

    def vertical(i):
        return segs[i][0][0] == segs[i][1][0]

    def is_bend(pt):
        meeting = ends[pt]
        return len(meeting) == 2 and vertical(meeting[0]) != vertical(meeting[1])

    return {
        "segments": len(segs),
        "bends": sum(1 for pt in ends if is_bend(pt)),
        "wirelength": round(
            sum(abs(p1[0] - p2[0]) + abs(p1[1] - p2[1]) for p1, p2 in segs), 2
        ),
    }


def score_files(*paths):
    """Score and total the .kicad_sch files in the given files or directories.

    Args:
        paths (str): .kicad_sch files or directories containing them.

    Returns:
        dict: Summed scores across all the files.
    """
    total = {"segments": 0, "bends": 0, "wirelength": 0.0}
    for path in paths:
        files = (
            sorted(glob.glob(os.path.join(path, "*.kicad_sch")))
            if os.path.isdir(path)
            else [path]
        )
        for f in files:
            with open(f) as fp:
                for key, value in score_schematic(fp.read()).items():
                    total[key] += value
    total["wirelength"] = round(total["wirelength"], 2)
    return total


if __name__ == "__main__":
    for p in sys.argv[1:]:
        print(p, score_files(p))
