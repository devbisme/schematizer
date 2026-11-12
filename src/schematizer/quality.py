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
    part_overlaps  pairs of symbols whose drawings overlap
    text_overlaps  text items overlapping each other, another symbol or a wire

Overlaps come from .kicad_sch files only; SVG pages score zero for them.

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

from simp_sexp import Sexp

__all__ = ["score_schematic", "score_overlaps", "score_files"]

_CHAR_W = 0.6  # Text width per character, as a fraction of the font size.
_FONT_SIZE = 1.27  # mm.
_LABEL_H = 2.0  # Height of a global label's flag, mm.
_LABEL_END = 2.0  # Extra length of a global label's flag beyond its text, mm.
_TOUCH = 0.1  # Boxes must overlap by more than this, so touching pins don't count.

_WIRE_RE = re.compile(
    r"\(wire\s*\(pts\s*\(xy\s+([-\d.]+)\s+([-\d.]+)\)\s*\(xy\s+([-\d.]+)\s+([-\d.]+)\)"
)
_SVG_WIRE_RE = re.compile(
    r'<line x1="([-\d.]+)" y1="([-\d.]+)" x2="([-\d.]+)" y2="([-\d.]+)"'
)


def _wires(sch_text):
    """Extract wire segments, from either format, as ((x1, y1), (x2, y2)) tuples rounded to 0.01 mm."""
    return [
        ((round(float(x1), 2), round(float(y1), 2)), (round(float(x2), 2), round(float(y2), 2)))
        for x1, y1, x2, y2 in _WIRE_RE.findall(sch_text) + _SVG_WIRE_RE.findall(sch_text)
    ]


def score_schematic(sch_text):
    """Score the wiring in the text of one schematic file.

    Args:
        sch_text (str): Contents of a .kicad_sch or .svg file.

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


def _field(element, head):
    """The first sub-expression of an element that starts with *head*."""
    return next((e for e in element if isinstance(e, list) and e and e[0] == head), None)


def _fields(element, head):
    """All the sub-expressions of an element that start with *head*."""
    return [e for e in element if isinstance(e, list) and e and e[0] == head]


def _symbol_points(lib_symbol, unit):
    """Library-coordinate points outlining one unit of a symbol definition."""
    pts = []
    pin_names = _field(lib_symbol, "pin_names")
    show_names = not (pin_names and ("hide" in pin_names or _field(pin_names, "hide")))

    def add_graphics(sym):
        for item in sym:
            if not isinstance(item, list) or not item:
                continue
            if item[0] in ("polyline", "bezier"):
                pts.extend((xy[1], xy[2]) for xy in _fields(_field(item, "pts"), "xy"))
            elif item[0] in ("rectangle", "arc"):
                for head in ("start", "mid", "end"):
                    at = _field(item, head)
                    if at:
                        pts.append((at[1], at[2]))
            elif item[0] == "circle":
                (_, x, y), radius = _field(item, "center")[:3], _field(item, "radius")[1]
                pts.extend([(x - radius, y - radius), (x + radius, y + radius)])
            elif item[0] == "pin":
                _, x, y, angle = _field(item, "at")[:4]
                length = _field(item, "length")[1]
                dx, dy = {0: (1, 0), 90: (0, 1), 180: (-1, 0), 270: (0, -1)}[angle % 360]
                pts.extend([(x, y), (x + dx * length, y + dy * length)])
                name = _field(item, "name")
                if show_names and name and name[1] != "~":
                    # A pin's name is drawn alongside it, on either side.
                    pad_x, pad_y = dy * _FONT_SIZE, dx * _FONT_SIZE
                    pts.extend([(x + pad_x, y + pad_y), (x - pad_x, y - pad_y)])

    for sub in _fields(lib_symbol, "symbol"):
        # Sub-symbols are named NAME_UNIT_STYLE; unit 0 is common to all units.
        if sub[1].rsplit("_", 2)[1] in ("0", str(unit)):
            add_graphics(sub)
    return pts


def _instance_map(symbol):
    """Get the function that maps a symbol's library coordinates onto its sheet."""
    _, ox, oy, angle = _field(symbol, "at")[:4]
    mirror = _field(symbol, "mirror")
    mirror = mirror[1] if mirror else None
    cos, sin = {0: (1, 0), 90: (0, 1), 180: (-1, 0), 270: (0, -1)}[angle % 360]

    def to_sheet(x, y):
        y = -y  # Library coordinates are Y-up, sheets are Y-down.
        x, y = (x * cos + y * sin, -x * sin + y * cos)
        if mirror == "x":
            y = -y
        elif mirror == "y":
            x = -x
        return ox + x, oy + y

    return to_sheet


def _overlap(box1, box2):
    return (
        box1[0] + _TOUCH < box2[2]
        and box2[0] + _TOUCH < box1[2]
        and box1[1] + _TOUCH < box2[3]
        and box2[1] + _TOUCH < box1[3]
    )


def _crosses(box, wire):
    """True if a horizontal or vertical wire runs through a box."""
    return (
        wire[0] < box[2] - _TOUCH
        and box[0] + _TOUCH < wire[2] + _TOUCH * 2
        and wire[1] < box[3] - _TOUCH
        and box[1] + _TOUCH < wire[3] + _TOUCH * 2
    )


def _field_box(symbol, field, to_sheet):
    """Sheet box of a visible Reference/Value field, which reads horizontally."""
    _, x, y, angle = _field(field, "at")[:4]
    w = len(field[2]) * _FONT_SIZE * _CHAR_W
    h = _FONT_SIZE
    justify = _field(_field(field, "effects"), "justify")
    justify = next((j for j in (justify or [])[1:] if j in ("left", "right")), None)

    # The justification is relative to the symbol: see if the text's reading
    # direction comes out reversed on the sheet.
    origin = to_sheet(0, 0)
    tip = to_sheet(1, 0) if angle % 180 == 0 else to_sheet(0, 1)
    reversed_ = tip[0] < origin[0]
    if justify and reversed_:
        justify = "left" if justify == "right" else "right"

    if justify == "left":
        return x, y - h / 2, x + w, y + h / 2
    if justify == "right":
        return x - w, y - h / 2, x, y + h / 2
    return x - w / 2, y - h / 2, x + w / 2, y + h / 2


def _label_box(label):
    """Sheet box of a global label's flag, which extends away from its anchor."""
    _, x, y, angle = _field(label, "at")[:4]
    length = len(label[1]) * _FONT_SIZE * _CHAR_W + _LABEL_END
    half = _LABEL_H / 2
    return {
        0: (x, y - half, x + length, y + half),
        180: (x - length, y - half, x, y + half),
        90: (x - half, y - length, x + half, y),
        270: (x - half, y, x + half, y + length),
    }[angle % 360]


def score_overlaps(sch_text):
    """Count the overlapping symbols and text in the text of one .kicad_sch file.

    Text extents are estimates, since the real ones depend on the font.

    Args:
        sch_text (str): Contents of a .kicad_sch file.

    Returns:
        dict: Counts of part_overlaps and text_overlaps. Both are zero for
            anything that isn't a .kicad_sch file.
    """
    if not sch_text.lstrip().startswith("(kicad_sch"):
        return {"part_overlaps": 0, "text_overlaps": 0}

    sch = Sexp(sch_text)
    lib_symbols = {sym[1]: sym for sym in _fields(_field(sch, "lib_symbols") or [], "symbol")}

    bodies, texts = [], []  # Text is stored with its symbol's index, or None for a label.
    for kind in ("global_label", "hierarchical_label"):
        for label in _fields(sch, kind):
            texts.append((_label_box(label), None))
    for symbol in _fields(sch, "symbol"):
        lib_id = _field(symbol, "lib_id")[1]
        lib_symbol = lib_symbols.get(lib_id)
        if lib_symbol is None:
            continue
        unit = _field(symbol, "unit")
        to_sheet = _instance_map(symbol)
        pts = [to_sheet(x, y) for x, y in _symbol_points(lib_symbol, unit[1] if unit else 1)]
        if not pts:
            continue
        xs, ys = zip(*pts)
        index = len(bodies)
        bodies.append((min(xs), min(ys), max(xs), max(ys)))
        for field in _fields(symbol, "property"):
            hidden = _field(_field(field, "effects"), "hide")
            if field[1] in ("Reference", "Value") and field[2] and not hidden:
                texts.append((_field_box(symbol, field, to_sheet), index))

    part_overlaps = sum(
        _overlap(box1, box2)
        for i, box1 in enumerate(bodies)
        for box2 in bodies[i + 1 :]
    )
    # Text sits against its own symbol by design, so only other symbols count.
    text_overlaps = sum(
        _overlap(box, body)
        for box, owner in texts
        for i, body in enumerate(bodies)
        if i != owner
    )
    text_overlaps += sum(
        _overlap(box1, box2)
        for i, (box1, _) in enumerate(texts)
        for box2, _ in texts[i + 1 :]
    )
    # Labels are attached to wires, so only a part's text can collide with one.
    wires = [
        (min(x1, x2), min(y1, y2), max(x1, x2), max(y1, y2))
        for (x1, y1), (x2, y2) in _wires(sch_text)
    ]
    text_overlaps += sum(
        _crosses(box, wire) for box, owner in texts if owner is not None for wire in wires
    )
    return {"part_overlaps": part_overlaps, "text_overlaps": text_overlaps}


def score_files(*paths):
    """Score and total the schematic files in the given files or directories.

    Args:
        paths (str): Schematic files or directories containing them. A directory's
            .svg files are only scored if it has no .kicad_sch files, since both
            formats draw the same wires.

    Returns:
        dict: Summed scores across all the files.
    """
    total = {
        "segments": 0,
        "bends": 0,
        "wirelength": 0.0,
        "part_overlaps": 0,
        "text_overlaps": 0,
    }
    for path in paths:
        files = [path]
        if os.path.isdir(path):
            files = sorted(glob.glob(os.path.join(path, "*.kicad_sch"))) or sorted(
                glob.glob(os.path.join(path, "*.svg"))
            )
        for f in files:
            with open(f) as fp:
                text = fp.read()
            for scores in (score_schematic(text), score_overlaps(text)):
                for key, value in scores.items():
                    total[key] += value
    total["wirelength"] = round(total["wirelength"], 2)
    return total


if __name__ == "__main__":
    for p in sys.argv[1:]:
        print(p, score_files(p))
