# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""Use the routed wires to improve the placement: nudge parts to straighten staircases.

A wire that steps sideways between two pins (a staircase) means the pins are
almost, but not quite, in line. Wire cleanup can't remove it, because any
path between the two pins needs the step. Moving one of the parts by the
size of the step can: the pins line up and the wire runs straight.

After a sheet is routed, :func:`straighten` looks for such staircases, moves
the lighter part of each, and routes the sheet again. The new routing is
kept only if it has fewer bends and crossings; otherwise the parts go back
where they were.
"""

from collections import defaultdict

from ..geometry import BBox, Vector
from ..quality import count_crossings
from . import progress
from .rng import rng

__all__ = ["straighten", "find_staircases", "wiring_cost", "MAX_NUDGE", "MAX_ROUNDS", "MAX_TRIES"]

# The longest step (mils) a part is moved to remove. 3 grid steps, the same
# near-miss distance placement uses when it lines pins up.
MAX_NUDGE = 150

# How many times a sheet is nudged and routed again while that keeps helping.
MAX_ROUNDS = 3

# The most times a sheet is routed again in all, counting the tries that are undone.
MAX_TRIES = 8

# Parts with more wired pins than this aren't moved: straightening one wire
# of a part like that bends its others.
MAX_WIRED_PINS = 4


def _ends(segments):
    """Return {point: [segments ending there]}."""
    ends = defaultdict(list)
    for seg in segments:
        ends[seg.p1].append(seg)
        ends[seg.p2].append(seg)
    return ends


def _horizontal(seg):
    return seg.p1.y == seg.p2.y


def _is_corner(segs):
    """True if exactly two segments meet at a point, at a right angle."""
    return len(segs) == 2 and _horizontal(segs[0]) != _horizontal(segs[1])


def _other_end(seg, pt):
    return seg.p2 if seg.p1 == pt else seg.p1


def _pin_points(node):
    """Return {point: pin} for the pins of the node's parts."""
    return {
        (pin.pt * part.tx).round(): pin for part in node.parts for pin in part.pins
    }


def wiring_cost(node):
    """Return (bends plus crossings, wire length) of a node's wiring, for comparing two routings of it.

    A wire crossing another counts the same as a bend: either makes a wire
    harder to follow.
    """
    pin_pts = _pin_points(node)
    bends, length = 0, 0
    for segments in node.wires.values():
        for pt, segs in _ends(segments).items():
            if _is_corner(segs) and pt not in pin_pts:
                bends += 1
        for seg in segments:
            length += abs(seg.p2.x - seg.p1.x) + abs(seg.p2.y - seg.p1.y)
    crossings = count_crossings(
        [
            ((seg.p1.x, seg.p1.y), (seg.p2.x, seg.p2.y))
            for segments in node.wires.values()
            for seg in segments
        ]
    )
    return bends + crossings, length


def find_staircases(node):
    """Find the wires that step sideways on their way between two points.

    Returns:
        list: A tuple (net, near end, step start, step end, far end) for each
            staircase: the wire runs from the near end to the step start,
            steps sideways to the step end, and runs on to the far end in the
            direction it came.
    """
    pin_pts = _pin_points(node)
    stairs = []
    for net, segments in node.wires.items():
        ends = _ends(segments)

        def corner(pt):
            return _is_corner(ends[pt]) and pt not in pin_pts

        for mid in segments:
            if not (corner(mid.p1) and corner(mid.p2)):
                continue
            step = abs(mid.p2.x - mid.p1.x) + abs(mid.p2.y - mid.p1.y)
            if step > MAX_NUDGE:
                continue
            a = next(seg for seg in ends[mid.p1] if seg is not mid)
            b = next(seg for seg in ends[mid.p2] if seg is not mid)
            pa, pb = _other_end(a, mid.p1), _other_end(b, mid.p2)
            # The outer segments of a staircase leave the step in opposite
            # directions. (If they leave the same way it's a U, which moving
            # a part sideways doesn't fix.)
            da = (pa.x - mid.p1.x) + (pa.y - mid.p1.y)
            db = (pb.x - mid.p2.x) + (pb.y - mid.p2.y)
            if da * db < 0:
                stairs.append((net, pa, mid.p1, mid.p2, pb))
    return stairs


def _wired_pin_counts(node, pin_pts):
    """Return {part: number of its pins that have a wire on them}."""
    counts = defaultdict(int)
    wire_ends = {pt for segments in node.wires.values() for seg in segments for pt in (seg.p1, seg.p2)}
    for pt, pin in pin_pts.items():
        if pt in wire_ends:
            counts[pin.part] += 1
    return counts


def _plan_moves(node, stairs):
    """Choose a part to move for each staircase.

    Returns:
        tuple: ({part: Vector to move it by}, {part: number of wired pins}).
    """
    pin_pts = _pin_points(node)
    wired = _wired_pin_counts(node, pin_pts)

    wanted = defaultdict(set)
    for net, pa, ma, mb, pb in stairs:
        # Moving the part at one end by the step, or the part at the other end
        # by the step reversed, puts both ends in line.
        choices = []
        for pt, dx, dy in ((pa, mb.x - ma.x, mb.y - ma.y), (pb, ma.x - mb.x, ma.y - mb.y)):
            pin = pin_pts.get(pt)
            if pin is not None and pin.net is net:
                choices.append((wired[pin.part], len(pin.part.pins), str(pin.part.ref), pin.part, (dx, dy)))
        if choices:
            # Move the part that has the least else to disturb.
            *_, part, move = min(choices, key=lambda c: c[:3])
            wanted[part].add(move)

    # A part asked to go two ways stays put.
    moves = {part: Vector(*next(iter(vecs))) for part, vecs in wanted.items() if len(vecs) == 1}
    return moves, wired


def _part_bbox(part, tx=None):
    return getattr(part, "lbl_bbox", part.bbox) * (part.tx if tx is None else tx)


def _apply_moves(node, moves):
    """Move the parts that have room to move. Return the ones moved."""
    limit = None
    if node.flattened:
        # A flattened node is drawn on its parent's sheet in the space it was
        # given at placement, so its parts must stay inside that.
        limit = BBox()
        for part in node.parts:
            limit.add(_part_bbox(part))

    moved = []
    for part, vec in sorted(moves.items(), key=lambda kv: str(kv[0].ref)):
        tx = part.tx.move(vec)
        bbox = _part_bbox(part, tx)
        if limit is not None and (
            bbox.min.x < limit.min.x or bbox.min.y < limit.min.y
            or bbox.max.x > limit.max.x or bbox.max.y > limit.max.y
        ):
            continue
        others = [_part_bbox(p) for p in node.parts if p is not part]
        others += [child.bbox * child.tx for child in node.children.values()]
        if any(bbox.intersects(other) for other in others):
            continue
        part.tx = tx
        moved.append(part)
    return moved


def _try_moves(node, moves, tool, rng_state, options):
    """Move parts and route again. Keep the result if it has fewer bends and crossings, else undo it."""
    from .route import RoutingFailure

    saved_tx = {part: part.tx for part in node.parts}
    saved_wires, saved_junctions = node.wires, node.junctions
    before = wiring_cost(node)

    def undo():
        for part, tx in saved_tx.items():
            part.tx = tx
        node.wires, node.junctions = saved_wires, saved_junctions

    if not _apply_moves(node, moves):
        return False
    if rng_state is not None:
        # The router breaks ties randomly. Starting it where the first routing
        # started keeps the wires away from the moved parts as they were, so
        # the comparison is about the moves and not the luck of the draw.
        rng.setstate(rng_state)
    try:
        with progress.prefixed("straightening wires,"):
            node.route_parts(tool=tool, **options)
    except RoutingFailure:
        undo()
        return False
    if wiring_cost(node) < before:
        return True
    undo()
    return False


def straighten(node, tool=None, rng_state=None, **options):
    """Nudge parts of a routed node to straighten its staircase wires, and route it again.

    Args:
        node (SchNode): A node whose own parts have just been routed.
        tool (str): Backend tool for schematics.
        rng_state: The random generator's state when the node's routing began.
        options (dict): The routing options the node was routed with.

    Returns:
        int: The number of times the node was nudged and its new routing kept.
    """
    kept = 0
    tries = 0
    rejected = set()

    def attempt(some):
        nonlocal tries, kept
        tries += 1
        if _try_moves(node, some, tool, rng_state, options):
            kept += 1
            return True
        if len(some) == 1:
            rejected.update((part, (vec.x, vec.y)) for part, vec in some.items())
        return False

    for _ in range(MAX_ROUNDS):
        stairs = find_staircases(node)
        if not stairs:
            break
        moves, wired = _plan_moves(node, stairs)
        moves = {
            part: vec for part, vec in moves.items()
            if (part, (vec.x, vec.y)) not in rejected and wired[part] <= MAX_WIRED_PINS
        }
        if not moves or tries >= MAX_TRIES:
            break
        if attempt(moves):
            continue
        if len(moves) == 1:
            break
        # One bad move spoils the lot, so try them one at a time, starting
        # with the parts that have the least else to disturb.
        improved = False
        for part in sorted(moves, key=lambda p: (wired[p], str(p.ref))):
            if tries >= MAX_TRIES:
                break
            improved |= attempt({part: moves[part]})
        if not improved:
            break
    return kept
