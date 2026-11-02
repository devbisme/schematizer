# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""
Write a placed-and-routed circuit out as SVG pages.

This is the sibling of :mod:`sexp_schematic`: same input (a placed + routed
``SchNode`` tree), same page geometry, different serialization. Where the
s-expression writer emits a symbol *instance* that KiCad later draws from its
``lib_symbols`` section, this module draws the symbol graphics itself using
:mod:`svg_draw`. Labels, wires and junctions aren't decided here: both writers
draw the list built by ``sexp_schematic.sheet_connectivity``.

Hierarchy works exactly as it does for KiCad, and is driven by the same
``flatness`` option:

* A **flattened** node contributes its elements to its parent's page.
* An **unflattened** node gets its own ``.svg`` file, and the parent draws a
  rectangle carrying the child's name -- hyperlinked to the child's file, so
  clicking it opens the child sheet.
* Each child page carries links back to its parent and to the top sheet.

So ``flatness=0`` gives a fully hierarchical set of linked pages, ``flatness=1``
gives a single flat page, and anything between gives a partially collapsed
hierarchy -- without this module needing to know which it is.
"""

import math
import os

from ..geometry import BBox, Point, Tx
from .._utils import export_to_all
from .net_terminal import NetTerminal
from .svg_draw import COLORS, draw_cmd_to_svg, esc

# ``init_power_symbol_data()`` REBINDS sexp_schematic's power-symbol globals, so
# they are reached through the module rather than imported by value -- a
# by-value import would freeze the empty dict created at import time.
from . import sexp_schematic as _sexp
from .sexp_schematic import (
    A_SIZES,
    _calc_sheet_tx,
    _round_mm,
    calc_field_layout,
    calc_pin_dir,
    init_power_symbol_data,
)

__all__ = ["write_top_svg"]

# Placement coordinates are mils; symbol graphics are mm.
MM_TO_MILS = 1 / 0.0254

# S-expression heads that are drawable graphics. Used to pull draw commands out
# of a nested lib_symbol definition (KiCad buries them in "unit" sub-symbols).
_DRAW_HEADS = {"polyline", "rectangle", "circle", "arc", "pin", "text", "property"}

# Wire/label line weights, in mm.
_WIRE_WIDTH = 0.1524
_JUNCTION_RADIUS = 0.45
_NO_CONNECT_SIZE = 0.635


# ---------------------------------------------------------------------------
# Transforms
# ---------------------------------------------------------------------------


def symbol_render_tx(part, sheet_tx):
    """Map a part's symbol-local mm (Y-up) onto the page in mm (Y-down).

    Scaling symbol mm up to placement mils first means this is the *same*
    composition the wire and label emitters use for pin positions
    (``pin.pt * part.tx * sheet_tx``, where ``pin.pt`` is the pin's mm
    coordinates already scaled to mils). That is what guarantees the drawn
    symbol's pins land exactly on the ends of the routed wires.
    """
    return Tx(a=MM_TO_MILS, d=MM_TO_MILS) * getattr(part, "tx", Tx()) * sheet_tx


def instance_tx(angle_deg, origin):
    """Transform for a symbol placed at *origin* (page mm) rotated *angle_deg*.

    Mirrors how KiCad renders a symbol instance: flip Y (symbol space is Y-up,
    the page is Y-down), rotate by the negated angle because the flip reverses
    rotation, then translate. Used for power symbols, which are placed
    directly at a pin rather than by the placer.
    """
    return Tx(a=1, d=-1).rot(-angle_deg).move(origin)


# ---------------------------------------------------------------------------
# Symbol drawing
# ---------------------------------------------------------------------------


def _collect_draw_cmds(sexp):
    """Recursively pull drawable commands out of a lib_symbol definition."""
    cmds = []
    for item in sexp:
        if isinstance(item, list) and item and isinstance(item[0], str):
            if item[0] in _DRAW_HEADS:
                cmds.append(item)
            elif item[0] == "symbol":
                cmds.extend(_collect_draw_cmds(item))
    return cmds


def _part_draw_cmds(part):
    """Draw commands for the unit of *part* that is actually placed.

    ``draw_cmds`` is keyed by unit number, with unit 0 holding the graphics
    common to every unit, so both are drawn.
    """
    draw_cmds = getattr(part, "draw_cmds", None) or {}
    unit_num = getattr(part, "num", 1)
    cmds = []
    for key in (0, unit_num):
        cmds.extend(draw_cmds.get(key, []))
    return cmds


def _halo_text(cls, text, x, y, size, color, anchor="middle", rotate=0):
    """Draw text over a white backing so it stays readable across a wire.

    The backing is a second copy of the text drawn underneath with a fat white
    stroke. The obvious ``paint-order="stroke"`` does the same thing in one
    element, but it is unsupported by several SVG renderers, which then paint
    the stroke OVER the glyphs and erase the text -- so the two-pass form is
    used instead. KiCad avoids these collisions by auto-placing fields when the
    editor opens the file; nothing moves them in a static page.
    """
    common = (
        f'x="{x:.3f}" y="{y:.3f}" '
        f'transform="rotate({rotate} {x:.3f} {y:.3f})" '
        f'dominant-baseline="central" text-anchor="{anchor}"'
    )
    text = esc(text)
    return (
        f'<text class="{cls} halo" {common} '
        f'style="font-size:{size}px; fill:#FFFFFF; stroke:#FFFFFF; '
        f'stroke-width:0.3; stroke-linejoin:round">{text}</text>\n'
        f'<text class="{cls}" {common} '
        f'style="font-size:{size}px; fill:{color}">{text}</text>\n'
    )


def _text_extent(text, x, y, size, anchor="middle", rotate=0):
    """Approximate the box a text run occupies, so page fitting reserves room.

    Without this the fitted page would clip labels and field text, since the
    anchor point alone says nothing about which way the glyphs extend.
    """
    w = len(str(text)) * size * 0.6
    h = size
    if anchor == "start":
        a, b = 0.0, w
    elif anchor == "end":
        a, b = -w, 0.0
    else:
        a, b = -w / 2, w / 2
    if rotate:
        # Rotated -90: the run climbs the page (toward smaller y).
        return BBox(Point(x - h / 2, y - b), Point(x + h / 2, y - a))
    return BBox(Point(x + a, y - h / 2), Point(x + b, y + h / 2))


def _field_svg(text, x, y, anchor="middle", rotate=0):
    """Draw a reference or value field."""
    return _halo_text("field", text, x, y, 1.27, COLORS["field"], anchor, rotate)


_SVG_ANCHOR = {"left": "start", "right": "end", None: "middle"}


def _field_positions(part, sheet_tx):
    """Where to put the reference and value text: the KiCad writer's layout
    (pin-free sides of the body), converted to SVG text anchors.

    Returns ``(ref_pos, value_pos)``, each ``(x, y, anchor, rotate)``.
    """
    return tuple(
        (x, y, _SVG_ANCHOR[justify], 0)
        for x, y, justify in calc_field_layout(part, sheet_tx)
    )


def part_to_svg(part, sheet_tx):
    """Draw a placed part's symbol. Returns ``(svg, bbox)``."""
    tx = symbol_render_tx(part, sheet_tx)

    # All units of a multi-unit part share one reference; the placed object may
    # be a unit whose own ref is the compound "U1.uA", so prefer the parent's.
    base_ref = getattr(getattr(part, "parent", None), "ref", None) or part.ref

    svg = [f'<g class="part" data-ref="{esc(base_ref)}">\n']
    bbox = BBox()
    for cmd in _part_draw_cmds(part):
        # Reference/Value are drawn below from the INSTANCE, not from the
        # symbol definition: a definition's field sits wherever the library
        # author put it (dead centre of the body, for Device:R), while the
        # KiCad writer re-places both fields relative to the part origin.
        # Drawing the definition's copy would disagree with the .kicad_sch.
        s, bb = draw_cmd_to_svg(cmd, tx)
        if s:
            svg.append(s)
            bbox.add(bb)

    if math.isfinite(bbox.w) and math.isfinite(bbox.h):
        ref_pos, val_pos = _field_positions(part, sheet_tx)
        value = str(getattr(part, "value", "") or "")
        for text, (x, y, anchor, rotate) in (
            (base_ref, ref_pos),
            (value, val_pos),
        ):
            if not text:
                continue
            svg.append(_field_svg(text, x, y, anchor, rotate))
            bbox.add(_text_extent(text, x, y, 1.27, anchor, rotate))

    svg.append("</g>\n")
    return "".join(svg), bbox


def power_symbol_to_svg(net_name, x, y, angle):
    """Draw a power symbol (GND, +3V3, ...) at (x, y). Returns ``(svg, bbox)``."""
    sym = _sexp.pwr_symbol_sexp_dict.get(net_name)
    if sym is None:
        return "", BBox()

    tx = instance_tx(angle, Point(x, y))

    svg = [f'<g class="power" data-net="{esc(net_name)}">\n']
    bbox = BBox()
    for cmd in _collect_draw_cmds(sym):
        # The power symbol's own Value property carries the net name; its
        # Reference is a hidden "#PWR" designator that KiCad never shows.
        s, bb = draw_cmd_to_svg(cmd, tx, field_text={"value": net_name})
        if s:
            svg.append(s)
            bbox.add(bb)
    svg.append("</g>\n")
    return "".join(svg), bbox


# ---------------------------------------------------------------------------
# Wires, junctions, labels
# ---------------------------------------------------------------------------


def wire_to_svg(x1, y1, x2, y2):
    """Draw a wire segment. Returns ``(svg, bbox)``."""
    svg = (
        f'<line x1="{x1:.3f}" y1="{y1:.3f}" x2="{x2:.3f}" y2="{y2:.3f}" '
        f'style="stroke:{COLORS["wire"]}; stroke-width:{_WIRE_WIDTH}" />\n'
    )
    return svg, BBox(Point(x1, y1), Point(x2, y2))


def junction_to_svg(x, y):
    """Draw a junction dot. Returns ``(svg, bbox)``."""
    svg = (
        f'<circle cx="{x:.3f}" cy="{y:.3f}" r="{_JUNCTION_RADIUS}" '
        f'style="fill:{COLORS["junction"]}; stroke:none" />\n'
    )
    return svg, BBox(Point(x, y))


def no_connect_to_svg(x, y):
    """Draw a no-connect cross. Returns ``(svg, bbox)``."""
    r = _NO_CONNECT_SIZE
    svg = (
        f'<path class="no_connect" d="M{x - r:.3f} {y - r:.3f} L{x + r:.3f} {y + r:.3f} '
        f'M{x - r:.3f} {y + r:.3f} L{x + r:.3f} {y - r:.3f}" '
        f'style="stroke:{COLORS["no_connect"]}; stroke-width:{_WIRE_WIDTH}; fill:none" />\n'
    )
    return svg, BBox(Point(x - r, y - r), Point(x + r, y + r))


# How a label's KiCad angle runs on the page: (rotate, text-anchor, offset).
# The label's anchor is its connection point and the text leads away from it.
_SVG_LABEL_PLACEMENT = {
    0: (0, "start", (0.8, 0.0)),
    180: (0, "end", (-0.8, 0.0)),
    90: (-90, "start", (0.0, -0.8)),
    270: (-90, "end", (0.0, 0.8)),
}


def net_label_to_svg(name, x, y, angle):
    """Draw a net label anchored at (x, y). Returns ``(svg, bbox)``."""
    rotate, anchor, off = _SVG_LABEL_PLACEMENT[angle]
    x, y = _round_mm(x + off[0]), _round_mm(y + off[1])
    svg = _halo_text(
        "net_label", name, x, y, 1.27, COLORS["global_label"], anchor, rotate
    )
    return svg, _text_extent(name, x, y, 1.27, anchor, rotate)


def _field(element, head):
    """The sub-expression of a sheet element that starts with *head*."""
    return next(
        (s for s in element if isinstance(s, list) and len(s) and s[0] == head), None
    )


def element_to_svg(element):
    """Draw one entry of a sheet's element list. Returns ``(svg, bbox)``.

    Only connectivity is drawn. Parts and sheet boxes aren't in the list given
    to this writer, and a hierarchical label has no drawing of its own: the
    parent page lists those nets on the sheet box.
    """
    kind = element[0]
    if kind == "wire":
        xys = [xy for xy in _field(element, "pts")[1:] if xy[0] == "xy"]
        return wire_to_svg(xys[0][1], xys[0][2], xys[1][1], xys[1][2])
    if kind == "junction":
        return junction_to_svg(*_field(element, "at")[1:3])
    if kind == "no_connect":
        return no_connect_to_svg(*_field(element, "at")[1:3])
    if kind == "global_label":
        return net_label_to_svg(element[1], *_field(element, "at")[1:4])
    if kind == "symbol":
        lib_id = _field(element, "lib_id")[1]
        return power_symbol_to_svg(
            lib_id.split(":", 1)[1], *_field(element, "at")[1:4]
        )
    return "", BBox()


# ---------------------------------------------------------------------------
# Hierarchical sheet boxes and navigation
# ---------------------------------------------------------------------------


def svg_filename(node):
    """The ``.svg`` filename for a node's sheet."""
    base = node.sheet_filename or "no_sheet_filename"
    return os.path.splitext(base)[0] + ".svg"


def sheet_box_to_svg(node, sheet_tx):
    """Draw a child sheet as a hyperlinked rectangle on its parent's page.

    The rectangle is the same ``external_bbox`` the placer reserved for the
    child, so the box sits exactly where the parent's routing expects it. The
    whole box is wrapped in an ``<a>`` pointing at the child's SVG file, so
    clicking it opens that sheet.
    """
    bbox = node.bbox * node.tx * sheet_tx
    x = _round_mm(bbox.ll.x)
    y = _round_mm(bbox.ll.y)
    w = _round_mm(abs(bbox.w))
    h = _round_mm(abs(bbox.h))
    href = svg_filename(node)

    svg = [
        f'<a href="{href}" target="_top">\n',
        '<g class="sheet">\n',
        f'<rect x="{x:.3f}" y="{y:.3f}" width="{w:.3f}" height="{h:.3f}" '
        f'style="stroke:{COLORS["sheet"]}; stroke-width:0.3; '
        f'fill:{COLORS["sheet_fill"]}" />\n',
        f'<text x="{x:.3f}" y="{y - 1.0:.3f}" '
        f'style="font-size:2.0px; fill:{COLORS["sheet"]}; font-weight:bold" '
        f'text-anchor="start">{esc(node.name)}</text>\n',
        f'<text x="{x:.3f}" y="{y + h + 2.4:.3f}" '
        f'style="font-size:1.4px; fill:{COLORS["sheet"]}" '
        f'text-anchor="start">{esc(href)}</text>\n',
    ]
    bbox = BBox(Point(x, y - 3.0), Point(x + w, y + h + 3.4))

    # The nets crossing into the child, listed down the inside of the left edge
    # -- the SVG counterpart of the sheet pins the KiCad writer emits. Without
    # them a parent page is just unlabelled boxes with no hint of what connects
    # to what. Same exclusions as create_hierarchical_sheet_sexp(): power nets
    # become power symbols inside the child, and stubbed nets travel by name.
    if hasattr(node, "get_boundary_nets"):
        pin_y = y + 2.54
        for net in node.get_boundary_nets():
            if net.name in _sexp.pwr_symbol_names:
                continue
            if getattr(net, "stub", False) or getattr(net, "_stub", False):
                continue
            if pin_y > y + h - 1.0:
                break  # Ran out of edge; the rest stay implicit.
            svg.append(
                f'<text class="sheet_pin" x="{x + 1.0:.3f}" y="{pin_y:.3f}" '
                f'style="font-size:1.27px; fill:{COLORS["sheet"]}" '
                f'dominant-baseline="central" '
                f'text-anchor="start">{esc(net.name)}</text>\n'
            )
            pin_y += 2.54

    svg.append("</g>\n</a>\n")
    return "".join(svg), bbox


def _nav_links_svg(node, parent, top_node):
    """Links back up the hierarchy, drawn at the top-left of a child page."""
    if parent is None and top_node is node:
        return ""

    links = []
    if parent is not None:
        links.append((f"▲ Up: {parent.name or 'top'}", svg_filename(parent)))
    if top_node is not None and top_node is not parent and top_node is not node:
        links.append((f"⌂ Top: {top_node.name or 'top'}", svg_filename(top_node)))

    svg = ['<g class="nav">\n']
    x = 8.0
    for text, href in links:
        svg.append(
            f'<a href="{href}" target="_top">'
            f'<text x="{x:.3f}" y="6.0" style="font-size:3.0px; '
            f'fill:{COLORS["sheet"]}; text-decoration:underline">{esc(text)}</text></a>\n'
        )
        x += max(28.0, len(text) * 1.9)
    svg.append("</g>\n")
    return "".join(svg)


_MARGIN = 5.0  # Page margin, mm.
_TITLE_W, _TITLE_H = 80.0, 20.0  # Title block size, mm.
_NAV_H = 10.0  # Height of the navigation strip, mm.


def _title_block_svg(node, title, page_w, page_h, top=0.0):
    """Page border plus a KiCad-style title block in the bottom-right corner.

    ``top`` is where the border starts, leaving room for the navigation strip
    above it.
    """
    m = _MARGIN
    bw, bh = _TITLE_W, _TITLE_H
    bx, by = page_w - m - bw, page_h - m - bh
    color = COLORS["border"]
    return "".join(
        [
            '<g class="frame">\n',
            f'<rect x="{m}" y="{max(m, top):.3f}" width="{page_w - 2 * m:.3f}" '
            f'height="{page_h - m - max(m, top):.3f}" '
            f'style="stroke:{color}; stroke-width:0.3; fill:none" />\n',
            f'<rect x="{bx:.3f}" y="{by:.3f}" width="{bw}" height="{bh}" '
            f'style="stroke:{color}; stroke-width:0.3; fill:none" />\n',
            f'<text x="{bx + 2:.3f}" y="{by + 6:.3f}" '
            f'style="font-size:3.5px; fill:{color}">{esc(title)}</text>\n',
            f'<text x="{bx + 2:.3f}" y="{by + 12:.3f}" '
            f'style="font-size:2.5px; fill:{color}">Sheet: {esc(node.name or "/")}</text>\n',
            f'<text x="{bx + 2:.3f}" y="{by + 17:.3f}" '
            f'style="font-size:2.5px; fill:{color}">File: {esc(svg_filename(node))}</text>\n',
            "</g>\n",
        ]
    )


def _page_geometry(bbox, paper, has_nav, fit):
    """Decide the page size and where the drawing sits on it.

    Returns ``(width, height, dx, dy)`` in mm, where ``(dx, dy)`` translates
    the drawing into place.

    With ``fit`` the page is shrunk to the drawing plus a margin, a navigation
    strip and a title block. That matters because an SVG is viewed at whatever
    size it declares -- a three-part circuit centred on A4, which is what the
    KiCad writer produces, would render as a speck. The element coordinates
    are NOT rescaled, only translated, so the drawing stays geometrically
    identical to the .kicad_sch for the same circuit.

    Without ``fit`` the drawing keeps its position on a real A-size page.
    """
    page_w, page_h = A_SIZES[paper]
    if not fit:
        return page_w, page_h, 0.0, 0.0

    w, h = bbox.w, bbox.h
    if not (math.isfinite(w) and math.isfinite(h)):
        # Nothing was drawn (an empty sheet); fall back to the paper size.
        return page_w, page_h, 0.0, 0.0

    nav_h = _NAV_H if has_nav else 0.0
    content_w = w + 2 * _MARGIN
    content_h = h + 2 * _MARGIN
    total_w = max(content_w, _TITLE_W + 2 * _MARGIN)
    total_h = nav_h + content_h + _TITLE_H + _MARGIN
    # Centre the drawing horizontally if the title block set the width.
    dx = -bbox.min.x + _MARGIN + (total_w - content_w) / 2
    dy = -bbox.min.y + _MARGIN + nav_h
    return total_w, total_h, dx, dy


_STYLE = """
  text { font-family: "DejaVu Sans", Verdana, Arial, sans-serif; }
  a { cursor: pointer; }
  g.sheet:hover rect { stroke-width: 0.6; }
"""


# ---------------------------------------------------------------------------
# Recursive hierarchy walker
# ---------------------------------------------------------------------------


def node_to_svg(node, parent=None, top_node=None, sheet_tx=Tx(), title="", fit=True):
    """Convert a node (and its children) to SVG.

    Mirrors ``node_to_sexp_schematic``: a flattened node returns its drawing
    elements for inclusion in the parent's page; an unflattened one writes its
    own file and returns a hyperlinked sheet box for the parent to draw.

    A sheet's labels and wires come from ``sexp_schematic.sheet_connectivity``,
    so they are the ones the KiCad writer emits for the same placement.

    Returns:
        tuple[str, BBox, list[str], list]: SVG fragment for the parent to
        include, its bounding box, the paths of every file written at or below
        this node, and a flattened node's connectivity elements for the
        parent's page.
    """
    if node.flattened:
        # No page of its own; draw straight into the parent's coordinate space.
        tx = node.tx * sheet_tx
        paper = None
    else:
        tx, paper = _calc_sheet_tx(node.internal_bbox())

    elements = []
    sheet_elements = []
    bbox = BBox()
    written = []

    def add(pair):
        svg, bb = pair
        if svg:
            elements.append(svg)
            bbox.add(bb)

    # Recurse into children first so their sheet boxes land under this page's
    # wires rather than on top of them.
    for child in node.children.values():
        child_svg, child_bbox, child_written, child_elements = node_to_svg(
            child, parent=node, top_node=top_node, sheet_tx=tx, title=title, fit=fit
        )
        written.extend(child_written)
        sheet_elements.extend(child_elements)
        if child_svg:
            elements.append(child_svg)
            bbox.add(child_bbox)

    for part in node.parts:
        if not isinstance(part, NetTerminal):
            add(part_to_svg(part, tx))

    sheet_elements = _sexp.sheet_connectivity(node, sheet_elements, tx)

    if node.flattened:
        # Hand the drawing up to the parent instead of writing a page.
        return "".join(elements), bbox, written, sheet_elements

    _sexp.deconflict_sheet_labels(sheet_elements, node, tx)
    for element in sheet_elements:
        add(element_to_svg(element))

    body = "".join(elements)

    # --- Unflattened node: write its own SVG page. ---
    nav = _nav_links_svg(node, parent, top_node)
    page_w, page_h, dx, dy = _page_geometry(bbox, paper, bool(nav), fit)
    page = [
        f'<svg xmlns="http://www.w3.org/2000/svg" '
        f'xmlns:xlink="http://www.w3.org/1999/xlink" '
        f'width="{page_w:.3f}mm" height="{page_h:.3f}mm" '
        f'viewBox="0 0 {page_w:.3f} {page_h:.3f}">\n',
        f"<style>{_STYLE}</style>\n",
        f'<rect width="{page_w:.3f}" height="{page_h:.3f}" '
        f'style="fill:#FFFFFF" />\n',
        _title_block_svg(node, title, page_w, page_h, top=_NAV_H if nav else 0.0),
        nav,
        f'<g transform="translate({dx:.3f} {dy:.3f})">\n',
        body,
        "</g>\n",
        "</svg>\n",
    ]

    filepath = os.path.join(node.filepath, svg_filename(node))
    with open(filepath, "w") as f:
        f.write("".join(page))
    written.append(filepath)

    # The parent draws a hyperlinked box standing in for this sheet.
    box_svg, box_bbox = sheet_box_to_svg(node, sheet_tx)
    return box_svg, box_bbox, written, []


@export_to_all
def write_top_svg(circuit, node, filepath, top_name, title, fit=True):
    """Write the complete schematic as one or more linked SVG pages.

    This is the SVG counterpart of ``write_top_schematic``.

    Args:
        circuit: The IR circuit (unused; kept parallel to the KiCad writer).
        node: Root SchNode, already placed and routed.
        filepath: Output directory.
        top_name: Base filename for the top sheet.
        title: Title-block text.
        fit: Shrink each page to its drawing instead of using a full A-size
            sheet. Default True, since an SVG is displayed at the size it
            declares. Geometry is unchanged either way.

    Returns:
        str: Path of the top-level SVG file.
    """
    init_power_symbol_data()

    node.title = title
    node.sheet_filename = top_name or "schematic"
    node.filepath = filepath

    # The top node has no parent to absorb its contents, so it always gets a page.
    node.flattened = False

    _, _, written, _ = node_to_svg(
        node, parent=None, top_node=node, title=title, fit=fit
    )

    top_file = os.path.join(filepath, svg_filename(node))
    return top_file if top_file in written else (written[-1] if written else "")
