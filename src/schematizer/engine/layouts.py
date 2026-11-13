# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""Save, compare and copy the layouts of hierarchical sheets.

A sheet's layout is where its parts are and how its wires run, for the sheet's
node and every node below it. Two things are done with one:

* The same subcircuit used twice (the two channels of a stereo amplifier)
  gets the same drawing. Once the first copy is placed and routed,
  :func:`reuse_layout` gives its layout to the second instead of laying that
  out from scratch, where it would come out looking different.
* A sheet is laid out several times and the best layout kept.
  :func:`best_layout` does the extra tries, comparing them by
  :func:`layout_cost`.

Both rely on a sheet being drawn as a box whose size doesn't depend on what's
inside it, so its insides can change without disturbing the sheet it's on.
"""

import copy
from collections import defaultdict

from ..geometry import Segment
from . import progress
from .net_terminal import NetTerminal

__all__ = ["Layout", "match_nodes", "reuse_layout", "best_layout", "layout_cost"]


# What placement leaves on a part: where it is, and the room it was given
# (which the writers use to keep labels off it).
_PART_ATTRS = ("tx", "place_bbox")


class Layout:
    """The placement and routing of a node and the nodes below it.

    Args:
        node (SchNode): The node to record.
    """

    def __init__(self, node):
        self.node = node
        self.part_attrs = {
            part: {attr: copy.copy(getattr(part, attr)) for attr in _PART_ATTRS if hasattr(part, attr)}
            for part in node.parts
        }
        self.wires = {
            net: [(copy.copy(seg.p1), copy.copy(seg.p2)) for seg in segments]
            for net, segments in node.wires.items()
        }
        self.junctions = {
            net: [copy.copy(pt) for pt in points] for net, points in node.junctions.items()
        }
        self.children = [
            (child, copy.copy(child.tx), copy.copy(child.bbox), Layout(child))
            for child in node.children.values()
        ]

    def apply(self, node=None, match=None):
        """Give this layout back to its node, or to another node that matches it.

        The node's own position and outline on its parent's sheet are left alone.

        Args:
            node (SchNode): The node to lay out. Defaults to the one recorded.
            match (Match): How the recorded node's parts, nets and nodes map
                onto those of ``node``, if it's a different one.
        """
        node = node or self.node
        part_of = match.parts.get if match else (lambda part: part)
        net_of = match.nets.get if match else (lambda net: net)
        node_of = match.nodes.get if match else (lambda child: child)

        for part, attrs in self.part_attrs.items():
            for attr, value in attrs.items():
                setattr(part_of(part), attr, copy.copy(value))
        node.wires = defaultdict(list)
        for net, segments in self.wires.items():
            node.wires[net_of(net)] = [
                Segment(copy.copy(p1), copy.copy(p2)) for p1, p2 in segments
            ]
        node.junctions = defaultdict(list)
        for net, points in self.junctions.items():
            node.junctions[net_of(net)] = [copy.copy(pt) for pt in points]
        for child, tx, bbox, layout in self.children:
            other = node_of(child)
            other.tx, other.bbox = copy.copy(tx), copy.copy(bbox)
            layout.apply(other, match)


class Match:
    """How the parts, nets and nodes of one node map onto those of another like it."""

    def __init__(self):
        self.parts = {}
        self.nets = {}
        self.nodes = {}
        self._nets_back = {}

    def pair_nets(self, net1, net2):
        """Pair two nets. Return False if either is already paired with another."""
        if self.nets.setdefault(net1, net2) is not net2:
            return False
        return self._nets_back.setdefault(net2, net1) is net1


def _box(bbox):
    if bbox is None:
        return None
    return tuple(round(v) for v in (bbox.min.x, bbox.min.y, bbox.max.x, bbox.max.y))


def _same_shape(part1, part2):
    """True if two parts are drawn the same and take up the same room."""
    if type(part1) is not type(part2) or len(part1.pins) != len(part2.pins):
        return False
    for attr in ("name", "num", "symtx", "orientation_locked"):
        if getattr(part1, attr, None) != getattr(part2, attr, None):
            return False
    for attr in ("bbox", "lbl_bbox"):
        if _box(getattr(part1, attr, None)) != _box(getattr(part2, attr, None)):
            return False
    for pin1, pin2 in zip(part1.pins, part2.pins):
        if (pin1.num, pin1.orientation, bool(pin1.stub)) != (
            pin2.num, pin2.orientation, bool(pin2.stub)
        ):
            return False
        if getattr(pin1, "pt", None) != getattr(pin2, "pt", None):
            return False
    return True


def _pair_pins(part1, part2, match):
    """Pair the nets on the pins of two parts. Return False if the nets can't be paired."""
    for pin1, pin2 in zip(part1.pins, part2.pins):
        net1, net2 = pin1.net, pin2.net
        if (net1 is None) != (net2 is None):
            return False
        if net1 is None:
            continue
        if type(net1) is not type(net2) or bool(getattr(net1, "stub", False)) != bool(
            getattr(net2, "stub", False)
        ):
            return False
        if not match.pair_nets(net1, net2):
            return False
    return True


def _match(node1, node2, match):
    if node1.flattened != node2.flattened or len(node1.children) != len(node2.children):
        return False

    # The parts of a subcircuit are made in the same order each time it's used.
    parts1 = [p for p in node1.parts if not isinstance(p, NetTerminal)]
    parts2 = [p for p in node2.parts if not isinstance(p, NetTerminal)]
    if len(parts1) != len(parts2):
        return False
    for part1, part2 in zip(parts1, parts2):
        if not _same_shape(part1, part2) or not _pair_pins(part1, part2, match):
            return False
        match.parts[part1] = part2

    for child1, child2 in zip(node1.children.values(), node2.children.values()):
        if not _match(child1, child2, match):
            return False
        match.nodes[child1] = child2

    # Net terminals are made in the order of the nets, which needn't agree,
    # so they're paired by the nets they label.
    terminals1 = {p.pins[0].net: p for p in node1.parts if isinstance(p, NetTerminal)}
    terminals2 = {p.pins[0].net: p for p in node2.parts if isinstance(p, NetTerminal)}
    if len(terminals1) != len(terminals2):
        return False
    for net1, terminal1 in terminals1.items():
        terminal2 = terminals2.get(match.nets.get(net1))
        if terminal2 is None or not _same_shape(terminal1, terminal2):
            return False
        match.parts[terminal1] = terminal2
    return True


def match_nodes(node1, node2):
    """See if two nodes hold the same circuit, drawn with the same symbols.

    They do if their parts pair off in order, each pair having the same
    symbol, size and pins, with the nets on those pins pairing off one to one,
    and the same goes for the nodes below them. Part values and net names can
    differ.

    Returns:
        Match: How the parts, nets and nodes of ``node1`` map onto those of
            ``node2``, or None if the nodes differ.
    """
    match = Match()
    if not _match(node1, node2, match):
        return None
    # Every wire of the first node must have a net to belong to in the second.
    stack = [node1]
    while stack:
        node = stack.pop()
        if any(net not in match.nets for net in node.wires):
            return None
        stack.extend(node.children.values())
    return match


def _sheet_nodes(node):
    """The node and the nodes below it that are drawn on its sheet."""
    nodes = [node]
    for child in node.children.values():
        if child.flattened:
            nodes.extend(_sheet_nodes(child))
    return nodes


def _all_nodes(node):
    nodes = [node]
    for child in node.children.values():
        nodes.extend(_all_nodes(child))
    return nodes


def reuse_layout(node, routed):
    """Give a node the layout of an earlier node that holds the same circuit.

    Args:
        node (SchNode): A placed node that's drawn on a sheet of its own.
        routed (list): Nodes already placed and routed.

    Returns:
        bool: True if the node took another's layout and needs no routing.
    """
    from .route import RoutingFailure

    if node.flattened or not any(n.parts for n in _all_nodes(node)):
        return False
    for other in routed:
        match = match_nodes(other, node)
        if not match:
            continue
        before = Layout(node)
        Layout(other).apply(node, match)
        try:
            for sub in _all_nodes(node):
                sub.check_pin_wires(sub.get_internal_nets())
        except RoutingFailure:
            before.apply()
            continue
        progress.report(f"drawing {node.name} like {other.name}")
        return True
    return False


def layout_cost(node):
    """Return (bends plus crossings, wire length) of the wiring on a node's sheet."""
    from .nudge import wiring_cost

    costs = [wiring_cost(n) for n in _sheet_nodes(node)]
    return sum(c[0] for c in costs), sum(c[1] for c in costs)


def best_layout(node, tries, tool=None, **options):
    """Lay out a routed sheet some more times and keep the layout with the lowest cost.

    Only what's drawn on the node's own sheet is laid out again. The sheets
    below it keep the layouts they have.

    Args:
        node (SchNode): A placed and routed node that's drawn on a sheet of its own.
        tries (int): How many layouts to choose from, counting the one it has.
        tool (str): Backend tool for schematics.
        options (dict): The placement and routing options, including the
            ``expansion_factor`` the node was last placed with.

    Returns:
        int: Which of the tries was kept, counting from 0.
    """
    from .place import PlacementFailure
    from .route import RoutingFailure

    if tries < 2 or node.flattened or not any(n.wires for n in _sheet_nodes(node)):
        return 0

    sheets_below = [n for n in _all_nodes(node) if not n.flattened and n is not node]
    kept = [Layout(sheet) for sheet in sheets_below]
    route_options = {k: v for k, v in options.items() if k != "expansion_factor"}

    best, best_cost, best_try = Layout(node), layout_cost(node), 0
    for i in range(1, tries):
        try:
            with progress.prefixed(f"layout {i + 1}/{tries},"):
                node.place(tool=tool, **options)
                # Placing the node places the sheets below it again too.
                for layout in kept:
                    layout.apply()
                node.route(tool=tool, route_sheets=False, **route_options)
        except (PlacementFailure, RoutingFailure):
            continue
        cost = layout_cost(node)
        if cost < best_cost:
            best, best_cost, best_try = Layout(node), cost, i
    best.apply()
    return best_try
