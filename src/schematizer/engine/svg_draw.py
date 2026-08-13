# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""
Render KiCad symbol draw commands as SVG.

The generic netlist embeds each symbol's ``draw_cmds`` -- the same
s-expression graphics KiCad keeps in ``lib_symbols`` -- so a symbol can be
drawn without any access to KiCad libraries. This module turns one draw
command into an SVG fragment plus its bounding box; :mod:`svg_schematic`
composes those into a page.

Coordinates in a draw command are symbol-local millimeters with Y pointing
UP. The caller supplies a ``Tx`` that maps them to page millimeters with Y
pointing DOWN (see ``svg_schematic.symbol_render_tx``), so this module never
needs to know where on the page a part sits.
"""

import math

from ..geometry import BBox, Point, Tx

__all__ = [
    "draw_cmd_to_dict",
    "draw_cmd_to_svg",
    "esc",
    "COLORS",
]

# KiCad's default schematic colors, so a rendered page looks like the
# .kicad_sch it was generated alongside.
COLORS = {
    "symbol": "#840000",  # Component outlines and pin lines.
    "symbol_fill": "#FFFFC2",  # "background"-filled shapes.
    "pin_name": "#0000C8",
    "field": "#840000",  # Reference / value text.
    "wire": "#008484",
    "junction": "#008484",
    "label": "#008484",
    "global_label": "#840000",
    "sheet": "#840084",  # Hierarchical sheet border + name.
    "sheet_fill": "#FFFFFF",
    "border": "#840000",  # Page border and title block.
    "text": "#000000",
}

# Text width per character, as a fraction of the font size. KiCad draws with a
# stroke font whose advance is close to this; SVG uses a real font, so this is
# only used for bounding boxes, never for placement.
CHAR_WIDTH_RATIO = 0.6


def draw_cmd_to_dict(symbol):
    """
    Convert a list of symbols from a KICAD part definition into a
    dictionary for easier access to properties.
    """
    name = symbol[0]
    items = symbol[1:]
    d = {}
    is_named_present = False
    item_names = []
    for item in items:
        # If the object is a list, recursively convert to dict
        if isinstance(item, list):
            item_name, item_dict = draw_cmd_to_dict(item)
            is_named_present = True
        # If the object is unnamed, put it in the "misc" list
        # ["key", item1, item2, ["xy", 0, 0]] -> "key": {"misc":[item1, item2], "xy":[0,0]
        else:
            item_name = "misc"
            item_dict = item

        # Multiple items with the same key (e.g. ("xy" 0 0) ("xy" 1 0))
        # get put into a list {"xy": [[0,0],[1,0]]}
        if item_name not in item_names:
            item_names.append(item_name)
        if item_name not in d:
            d[item_name] = [item_dict]
        else:
            d[item_name].append(item_dict)

    # if a list has only one item, remove it from the list
    for item_name in item_names:
        if len(d[item_name]) == 1:
            d[item_name] = d[item_name][0]

    if not is_named_present:
        d = d["misc"]

    return name, d


def esc(text):
    """Escape text for inclusion in XML character data or an attribute."""
    return (
        str(text)
        .replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace('"', "&quot;")
    )


def _text_bbox(text, start, dir, char_wid, char_hgt):
    """Approximate bounding box of a text run starting at *start* along *dir*."""
    char_wid *= CHAR_WIDTH_RATIO
    char_hgt *= CHAR_WIDTH_RATIO
    ortho_dir = dir * Tx().rot(90)
    p1 = start - ortho_dir * char_hgt / 2
    p2 = start + ortho_dir * char_hgt / 2
    p3 = p1 + dir * char_wid * len(str(text))
    p4 = p2 + dir * char_wid * len(str(text))
    return BBox(p1, p2, p3, p4)


def _side_of(vec):
    """Which side of the symbol a direction vector points toward.

    Used to pick text anchoring/rotation so labels read left-to-right (or
    bottom-to-top) no matter how the part was rotated during placement.
    """
    if vec.x > vec.y and vec.x > -vec.y:
        return "left"
    elif vec.x < vec.y and vec.x > -vec.y:
        return "top"
    elif vec.x < vec.y and vec.x < -vec.y:
        return "right"
    elif vec.x > vec.y and vec.x < -vec.y:
        return "bottom"
    # Degenerate vector (zero-length or exactly diagonal); anchor it like a
    # left-side label rather than failing the whole render.
    return "left"


# Anchor/rotation per side. Vertical sides are rotated -90 so text reads
# bottom-to-top, matching how KiCad draws rotated fields.
_TEXT_STYLE = {
    "left": (0, "start"),
    "right": (0, "end"),
    "top": (-90, "end"),
    "bottom": (-90, "start"),
}


def _text_svg(text, side, pt, char_wid, cls, color, anchor_override=None):
    """Emit a ``<text>`` element anchored per *side*."""
    rotate, anchor = _TEXT_STYLE[side]
    anchor = anchor_override or anchor
    return (
        f'<text class="{cls}" x="{pt.x:.3f}" y="{pt.y:.3f}" '
        f'transform="rotate({rotate} {pt.x:.3f} {pt.y:.3f})" '
        f'style="font-size:{char_wid:.3f}px; fill:{color}" '
        f'dominant-baseline="central" text-anchor="{anchor}">{esc(text)}</text>\n'
    )


def _fill_style(fill_type):
    """Map a KiCad fill type to an SVG fill color."""
    if fill_type == "background":
        return COLORS["symbol_fill"]
    if fill_type == "outline":
        return COLORS["symbol"]
    return "none"


def draw_cmd_to_svg(draw_cmd, tx, field_text=None, show_pin_numbers=False):
    """Convert one symbol draw command into SVG plus its bounding box.

    Args:
        draw_cmd (list): One s-expression graphics command from ``draw_cmds``.
        tx (Tx): Maps symbol-local mm (Y-up) to page mm (Y-down).
        field_text (dict, optional): Maps a lowercased property name
            (``"reference"``, ``"value"``) to the text to draw for this
            instance. A property with no entry is skipped, which is how
            hidden fields like Footprint and Datasheet are dropped.
        show_pin_numbers (bool): Draw pin numbers. Default False, matching the
            ``(pin_numbers hide)`` that the KiCad writer always emits.

    Returns:
        tuple[str, BBox]: SVG fragment and its bounding box in page mm. Both
        are empty for a command that draws nothing.
    """
    field_text = field_text or {}
    tx_scale = tx.scale

    def pts_str(*points):
        return " ".join(f"{p.x:.3f},{p.y:.3f}" for p in points)

    shape_type, shape = draw_cmd_to_dict(draw_cmd)

    # Normalize the optional stroke/fill sub-expressions so every branch can
    # read them unconditionally.
    stroke = shape.setdefault("stroke", {})
    if not isinstance(stroke, dict):
        stroke = shape["stroke"] = {}
    width = stroke.get("width", 0) or 0.1524
    fill = shape.get("fill", {})
    fill_type = fill.get("type", "none") if isinstance(fill, dict) else "none"

    stroke_width = abs(width * tx_scale)
    sym_color = COLORS["symbol"]

    if shape_type == "polyline":
        points = [Point(*pt[0:2]) * tx for pt in shape["pts"]["xy"]]
        bbox = BBox(*points)
        svg = (
            f'<polyline points="{pts_str(*points)}" '
            f'style="stroke:{sym_color}; stroke-width:{stroke_width:.3f}; '
            f'fill:{_fill_style(fill_type)}" />\n'
        )

    elif shape_type == "circle":
        ctr = Point(*shape["center"]) * tx
        r = abs(shape["radius"] * tx_scale)
        bbox = BBox(ctr + Point(r, r), ctr - Point(r, r))
        svg = (
            f'<circle cx="{ctr.x:.3f}" cy="{ctr.y:.3f}" r="{r:.3f}" '
            f'style="stroke:{sym_color}; stroke-width:{stroke_width:.3f}; '
            f'fill:{_fill_style(fill_type)}" />\n'
        )

    elif shape_type == "rectangle":
        bbox = BBox(Point(*shape["start"]) * tx, Point(*shape["end"]) * tx)
        svg = (
            f'<rect x="{bbox.min.x:.3f}" y="{bbox.min.y:.3f}" '
            f'width="{bbox.w:.3f}" height="{bbox.h:.3f}" '
            f'style="stroke:{sym_color}; stroke-width:{stroke_width:.3f}; '
            f'fill:{_fill_style(fill_type)}" />\n'
        )

    elif shape_type == "arc":
        a = Point(*shape["start"]) * tx
        b = Point(*shape["end"]) * tx
        c = Point(*shape["mid"]) * tx
        bbox = BBox(a, b, c)

        # Radius of the circle through the three points, via the triangle's
        # circumradius R = abc/4K.
        A = (b - c).magnitude
        B = (a - c).magnitude
        C = (a - b).magnitude
        try:
            angle = math.acos(max(-1.0, min(1.0, (A * A + B * B - C * C) / (2 * A * B))))
            K = 0.5 * A * B * math.sin(angle)
            r = A * B * C / 4 / K
            large_arc = int(math.pi / 2 > angle)
            sweep = int((b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x) < 0)
            path = (
                f"M {a.x:.3f} {a.y:.3f} "
                f"A {r:.3f} {r:.3f} 0 {large_arc} {sweep} {b.x:.3f} {b.y:.3f}"
            )
        except (ValueError, ZeroDivisionError):
            # Degenerate arc (collinear or coincident points): draw the chord
            # rather than dropping the stroke entirely.
            path = f"M {a.x:.3f} {a.y:.3f} L {b.x:.3f} {b.y:.3f}"
        svg = (
            f'<path d="{path}" '
            f'style="stroke:{sym_color}; stroke-width:{stroke_width:.3f}; '
            f'fill:{_fill_style(fill_type)}" />\n'
        )

    elif shape_type == "property":
        prop_name = str(shape["misc"][0]).lower()
        effects = shape.get("effects", {})
        text = field_text.get(prop_name)
        if text in (None, "") or "hide" in effects:
            # Hidden, or a field this page doesn't draw (Footprint, Datasheet).
            return "", BBox()
        start = Point(*shape["at"][0:2])
        rotation = shape["at"][2] if len(shape["at"]) > 2 else 0
        justify = effects.get("justify", "center")
        if isinstance(justify, list):
            justify = justify[0]
        dir = {
            "right": Point(-1, 0),
            "left": Point(1, 0),
            "center": Point(1, 0),
        }[str(justify).lower()] * Tx().rot(rotation)
        end = (start + dir) * tx
        start = start * tx
        side = _side_of(dir * tx.no_translate())
        char_wid, char_hgt = effects["font"]["size"][:]
        char_wid *= tx_scale
        char_hgt *= tx_scale
        svg = _text_svg(text, side, start, char_wid, "field", COLORS["field"])
        bbox = _text_bbox(text, start, (end - start).norm, char_wid, char_hgt)

    elif shape_type == "pin":
        start = Point(*shape["at"][0:2])
        rotation = shape["at"][2] if len(shape["at"]) > 2 else 0
        length = shape["length"]
        # In KiCad, a pin's "at" is its CONNECTION point and the line runs from
        # there toward the body.
        dir = Point(1, 0) * Tx().rot(rotation)
        end = (start + dir * length) * tx
        start = start * tx
        bbox = BBox(start, end)
        side = _side_of(dir * tx.no_translate())

        svg = (
            f'<polyline points="{pts_str(start, end)}" '
            f'style="stroke:{sym_color}; stroke-width:{stroke_width:.3f}; '
            f'fill:none" />\n'
        )

        pin_name = shape["name"]["misc"]
        name_wid, name_hgt = shape["name"]["effects"]["font"]["size"][:]
        name_wid *= tx_scale
        name_hgt *= tx_scale
        # "~" is KiCad's "no name"; it is never drawn.
        if pin_name and pin_name != "~" and "hide" not in shape["name"]["effects"]:
            svg += _text_svg(
                pin_name, side, end, name_wid, "pin_name", COLORS["pin_name"]
            )
            bbox += _text_bbox(pin_name, end, (end - start).norm, name_wid, name_hgt)

        if show_pin_numbers:
            pin_num = shape["number"]["misc"]
            num_wid, num_hgt = shape["number"]["effects"]["font"]["size"][:]
            num_wid *= tx_scale
            num_hgt *= tx_scale
            svg += _text_svg(
                pin_num, side, end, num_wid, "pin_num", COLORS["symbol"], "middle"
            )
            bbox += _text_bbox(pin_num, end, (end - start).norm, num_wid, num_hgt)

    elif shape_type == "text":
        start = Point(*shape["at"][0:2])
        rotation = shape["at"][2] if len(shape["at"]) > 2 else 0
        justify = shape.get("justify", "right")
        dir = {"right": Point(1, 0), "left": Point(-1, 0)}.get(
            str(justify).lower(), Point(1, 0)
        ) * Tx().rot(rotation)
        end = (start + dir) * tx
        start = start * tx
        side = _side_of(dir * tx.no_translate())
        char_wid, char_hgt = shape["effects"]["font"]["size"][:]
        char_wid *= tx_scale
        char_hgt *= tx_scale
        text = shape["misc"]
        svg = _text_svg(text, side, start, char_wid, "text", COLORS["text"])
        bbox = _text_bbox(text, start, (end - start).norm, char_wid, char_hgt)

    else:
        # Unknown command (e.g. a bezier from a newer KiCad): skip it rather
        # than abort the page. Everything else still renders.
        return "", BBox()

    return svg, bbox
