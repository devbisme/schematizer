# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""
Starting positions for force-directed placement.

Force-directed placement normally starts from random positions and spends its
first stages just pulling connected parts together. These heuristics put the
parts roughly where a person would instead, so placement can begin with the
stages that spread parts apart:

* Satellites: a two-pin part that hangs off the rest of the circuit by one pin
  (a pull-up, a series resistor, an LED with its resistor) is fixed beside the
  pin it's wired to. Its host carries it through placement as one block.
* Seed: each remaining part goes on the side of the pin it's wired to, with the
  two pins level so the wire between them is straight. With signals leaving
  parts on the right and entering on the left, that makes signals flow left to
  right.
"""

from collections import defaultdict
from itertools import combinations

import numpy as np

from ..geometry import BBox, Point, Tx, Vector
from .constants import GRID
from .conventions import best_orientations, linear, orientation_cost, orientations, pin_exit

__all__ = [
    "find_satellites",
    "attach_satellites",
    "release_satellites",
    "seed_placement",
    "spread_parts",
]

# Gap left between the placement boxes of two parts joined by a straight wire.
SEED_GAP = 2 * GRID

# How far (in grid steps) a part can be moved at a time to get it off of other
# parts, and the wire length a bend is worth when choosing where it goes.
SPREAD_MAX_STEPS = 20
SPREAD_BEND_COST = 4 * GRID
# Wire length it's worth for a wire not to have to double back behind its pin.
SPREAD_UTURN_COST = 8 * GRID
# Times each part gets to move again after all the parts are down.
SPREAD_PASSES = 2

# Search limits (in grid steps) for a satellite's spot beside its host pin.
SAT_MAX_OUT = 12
SAT_MAX_SIDE = 10

# Wire length (in grid steps) that a satellite's wire is penalized for
# stepping sideways, for each bend, and for doubling back on itself.
SAT_SIDE_COST = 3
SAT_BEND_COST = 2
SAT_UTURN_COST = 8

# Wire length (in grid steps) a satellite is penalized per unit of
# conventions.orientation_cost(). A pull-down lying on its side with a
# straight wire then beats an upright one whose wire has to go a long way.
SAT_CONVENTION_COST = 1.0

# Space kept between a satellite and other parts so the router can get a wire through.
SAT_CLEARANCE = GRID

# Length of the corridor kept clear where a wire leaves a part. It's as long
# as the satellite search is wide, so a satellite hanging from one pin never
# cuts across the wire from another pin on the same side.
SAT_EXIT_CLEARANCE = SAT_MAX_OUT * GRID


def _is_wired(pin, nets):
    """Return True if a pin gets a wire (connected, not stubbed, on a placed net)."""
    return pin.is_connected() and not pin.stub and pin.net in nets


def _wired_pins(part, nets):
    """Pins of a part that get wires."""
    return [pin for pin in part.pins if _is_wired(pin, nets)]


def find_satellites(parts, nets):
    """Find two-pin parts that can ride along beside the pin they're wired to.

    A satellite has one pin wired toward the rest of the circuit. Its other
    pin is unwired, or leads only to its own satellites (so an LED and its
    series resistor hang off an IC pin as a chain).

    Args:
        parts (list): Parts being placed (no NetTerminals).
        nets (list): Nets that will be wired between them.

    Returns:
        dict: Host part -> list of (satellite, its wired pin, the pin it's wired to).
            A satellite's pin may be wired to an earlier satellite in the list.
    """
    nets = set(nets)
    part_set = set(parts)
    wired_to = {}  # Satellite -> (its wired pin, the pin it's wired to).

    def other_pins(pin):
        """Wired pins on the pin's net that are on parts still in the circuit."""
        return [
            p
            for p in pin.net.pins
            if p.part is not pin.part
            and p.part in part_set
            and p.part not in wired_to
            and _is_wired(p, nets)
        ]

    # Strip satellites off the circuit until no more can be found. Removing
    # one can turn the part it was wired to into a satellite.
    found = True
    while found:
        found = False
        for part in parts:
            if part in wired_to or len(part.pins) != 2:
                continue
            if getattr(part, "orientation_locked", False):
                continue
            live = [pin for pin in _wired_pins(part, nets) if other_pins(pin)]
            if len(live) != 1:
                continue
            others = other_pins(live[0])
            # Parts on the same pin of an IC (a pull-up and a filter cap, say)
            # are satellites of the IC, not of each other.
            ics = [p for p in others if len(p.part.pins) > 2]
            hosts = ics or others
            if len(hosts) == 1:
                wired_to[part] = (live[0], hosts[0])
                found = True

    def host(part):
        while part in wired_to:
            part = wired_to[part][1].part
        return part

    # List each host's satellites so that each comes after the one it's wired to.
    satellites = defaultdict(list)
    done = set()

    def add(sat):
        if sat in done:
            return
        done.add(sat)
        sat_pin, host_pin = wired_to[sat]
        if host_pin.part in wired_to:
            add(host_pin.part)
        satellites[host(sat)].append((sat, sat_pin, host_pin))

    for part in parts:
        if part in wired_to:
            add(part)
    return satellites


def _inverse(tx):
    """Invert a transform made of rotations, flips and a translation."""
    lin = Tx(a=tx.a, b=tx.c, c=tx.b, d=tx.d)  # Transpose undoes a rotation/flip.
    back = Point(-tx.dx, -tx.dy) * lin
    return lin.move(back)


def _grow(bbox, amount):
    return BBox(bbox.min - Point(amount, amount), bbox.max + Point(amount, amount))


def _unit(vec):
    return Vector(round(vec.x), round(vec.y))


def _satellite_spots(sat, sat_pin, host_pt, out):
    """List the places a satellite could go beside the pin it's wired to.

    Orientations that break the drafting conventions cost extra, so a
    decoupling cap stays upright with its ground pin down, even with a bend in
    its wire, unless that would push it far from its pin.

    Args:
        sat (Part): The satellite.
        sat_pin (Pin): Its wired pin.
        host_pt (Point): Position of the pin it's wired to.
        out (Vector): Direction a wire leaves that pin.

    Yields:
        tuple: (transform for the satellite, corner of its wire or None), best first.
    """
    side = Vector(-out.y, out.x)
    spots = []
    lins = orientations(sat.tx)
    least_broken = min(orientation_cost(sat, lin) for lin in lins)
    for lin in lins:
        broken = (orientation_cost(sat, lin) - least_broken) * SAT_CONVENTION_COST
        exit_vec = _unit(pin_exit(sat_pin, lin))
        facing = exit_vec.x * out.x + exit_vec.y * out.y
        if facing < 0:
            # Pins face each other: a straight wire, or a jog if stepped sideways.
            offsets = [
                (k + SAT_SIDE_COST * abs(j), out * k + side * j, None)
                for k in range(2, SAT_MAX_OUT + 1)
                for j in range(-SAT_MAX_SIDE, SAT_MAX_SIDE + 1)
            ]
        elif facing == 0:
            # Pins at right angles: the wire goes out, then turns into the satellite.
            offsets = [
                (k + m + SAT_BEND_COST, out * k - exit_vec * m, out * k)
                for k in range(1, SAT_MAX_OUT + 1)
                for m in range(1, SAT_MAX_SIDE + 1)
            ]
        else:
            # Pins point the same way: the wire has to double back.
            offsets = [
                (k + abs(j) + SAT_UTURN_COST, out * k + side * j, None)
                for k in range(0, SAT_MAX_OUT + 1)
                for j in range(-SAT_MAX_SIDE, SAT_MAX_SIDE + 1)
                if abs(j) >= 2
            ]
        for cost, offset, corner in offsets:
            # Break ties the same way every time: positive side first.
            key = (cost + broken, offset.x < 0, offset.y < 0, len(spots))
            spots.append((key, lin, offset, corner))
    spots.sort(key=lambda spot: spot[0])

    # Only a few spots get looked at before one is free, so work out their
    # transforms as they're asked for.
    for _, lin, offset, corner in spots:
        shift = host_pt + offset * GRID - sat_pin.pt * lin
        yield lin.move(shift), None if corner is None else host_pt + corner * GRID


def attach_satellites(host, satellites, nets, expansion_factor=1.0):
    """Fix each satellite beside the pin it's wired to and grow the host to cover them.

    Each satellite gets a ``rel_tx`` that maps it into its host's coordinates,
    so after placement its transform is ``rel_tx * host.tx``. The satellite's
    wired pin sits a whole number of grid steps from the host pin, so it stays
    on grid when the host does.

    Args:
        host (Part): Part that will carry the satellites.
        satellites (list): Its entry from find_satellites().
        nets (list): Nets that will be wired between parts.
        expansion_factor (float): Multiplies the space left around satellites,
            for when routing failed and is being tried again with more room.
    """
    nets = set(nets)
    clearance = SAT_CLEARANCE * expansion_factor

    # Everything is laid out around the host as it's oriented now, with the
    # host's origin at (0, 0).
    host_lin = linear(host.tx)
    to_host = _inverse(host_lin)
    txs = {host: host_lin}

    def exit_corridor(pin):
        """Box covering where a wire leaves a pin and then its part's labels."""
        tx = txs[pin.part]
        pt = pin.pt * tx
        out = _unit(pin_exit(pin, tx))
        box = pin.part.lbl_bbox * tx
        reach = max(
            (box.max.x - pt.x) * out.x,
            (box.min.x - pt.x) * out.x,
            (box.max.y - pt.y) * out.y,
            (box.min.y - pt.y) * out.y,
        )
        tip = pt + out * (reach + SAT_EXIT_CLEARANCE)
        return _grow(BBox(pt, tip), GRID / 2)

    def wire_box(*pts):
        return _grow(BBox(*pts), GRID / 4)

    # Boxes that satellites have to keep out of. Each has an owner (a part, or
    # a pin for the wires from it) that's allowed to touch it. Satellites keep
    # their distance from parts, but their wires can run right alongside.
    blocked = [(host, _grow(host.lbl_bbox * host_lin, clearance))]
    wire_blocked = [(host, host.lbl_bbox * host_lin)]
    # Keep clear of where the host's wires leave it.
    for pin in _wired_pins(host, nets):
        corridor = exit_corridor(pin)
        blocked.append((pin, corridor))
        wire_blocked.append((pin, corridor))

    host.preplace_bbox = host.place_bbox
    bbox = BBox(host.place_bbox.min, host.place_bbox.max)
    host.riders = []
    for sat, sat_pin, host_pin in satellites:
        host_tx = txs[host_pin.part]
        host_pt = host_pin.pt * host_tx
        out = _unit(pin_exit(host_pin, host_tx))
        for tx, corner in _satellite_spots(sat, sat_pin, host_pt, out):
            box = sat.lbl_bbox * tx
            wire_pts = [host_pt, sat_pin.pt * tx] + ([] if corner is None else [corner])
            wire = wire_box(*wire_pts)
            # The wire has to touch the part it comes from and can share the
            # way with other wires from the same pin, but nothing else.
            if not any(
                box.intersects(b) for owner, b in blocked if owner is not host_pin
            ) and not any(
                wire.intersects(b)
                for owner, b in wire_blocked
                if owner is not host_pin and owner is not host_pin.part
            ):
                break
        # If every spot was blocked, the last (farthest) one is used and the
        # placer's overlap forces are left to deal with it.
        txs[sat] = tx
        sat.rel_tx = tx * to_host
        host.riders.append(sat)
        blocked.append((sat, _grow(box, clearance)))
        blocked.append((host_pin, wire))
        wire_blocked.append((sat, box))
        wire_blocked.append((host_pin, wire))
        for pin in _wired_pins(sat, nets):
            if pin is not sat_pin:
                corridor = exit_corridor(pin)
                blocked.append((pin, corridor))
                wire_blocked.append((pin, corridor))
        bbox.add(sat.place_bbox * sat.rel_tx)
    host.place_bbox = bbox


def release_satellites(hosts):
    """Put satellites where their placed hosts carried them, and shrink the hosts."""
    for host, satellites in hosts.items():
        host.place_bbox = host.preplace_bbox
        del host.preplace_bbox, host.riders
        for sat, _, _ in satellites:
            sat.tx = sat.rel_tx * host.tx
            del sat.rel_tx


def seed_placement(parts, nets):
    """Move parts to starting positions for force-directed placement.

    For each pair of wired pins on different parts, the second part wants to
    be where a short, straight wire would join the pins: on the side of the
    first part that its pin is on, with the two pins level. Those wants can't
    all be met, so the positions that come closest (in the least-squares
    sense) are found with one linear solve. Parts can end up on top of each
    other; spread_parts() fixes that.

    The pins must already have their place_pt set by add_anchor_pull_pins().

    Args:
        parts (list): Parts to position (no NetTerminals or satellites).
        nets (list): Nets that will be wired between them.
    """
    if len(parts) < 2:
        return

    index = {part: i for i, part in enumerate(parts)}
    net_set = set(nets)
    lins = [linear(part.tx) for part in parts]

    rows, wants = [], []
    for net in nets:
        pins = [pin for pin in net.pins if pin.part in index and _is_wired(pin, net_set)]
        num_parts = len({index[pin.part] for pin in pins})
        if num_parts < 2:
            continue
        # A net with many parts shouldn't pull harder than one with two.
        weight = (1.0 / (num_parts - 1)) ** 0.5
        for pin_a, pin_b in combinations(pins, 2):
            a, b = index[pin_a.part], index[pin_b.part]
            if a == b:
                continue
            exit_a = _unit(pin_exit(pin_a, lins[a]))
            exit_b = _unit(pin_exit(pin_b, lins[b]))
            # Part b goes in the direction pin a's wire leaves, which is also
            # the direction pin b's wire arrives from.
            toward = exit_a - exit_b
            if toward.magnitude:
                toward = toward / toward.magnitude
            want = pin_a.place_pt * lins[a] - pin_b.place_pt * lins[b] + toward * SEED_GAP
            row = np.zeros(len(parts))
            row[a], row[b] = -weight, weight
            rows.append(row)
            wants.append((want.x * weight, want.y * weight))

    if not rows:
        return

    # The least-norm solution centers the parts on the origin.
    pos = np.linalg.lstsq(np.array(rows), np.array(wants), rcond=None)[0]
    for part, lin, (x, y) in zip(parts, lins, pos):
        part.tx = lin.move(Vector(float(x), float(y)))


def _best_spot(part, tx, offsets, partners, boxes):
    """Find the free spot near a part's position that needs the least wiring.

    The wiring for a spot is estimated as the distance from each of the part's
    wired pins to the nearest pin it connects to, plus penalties if that wire
    needs a bend or has to double back behind its pin.

    Args:
        part (Part): Part being positioned.
        tx (Tx): Its transform at the position to search around.
        offsets (array): Moves from that position to try, nearest first.
        partners (dict): Pin of the part -> array of positions of the pins it's wired to.
        boxes (array): Placement bboxes (min x, min y, max x, max y) to stay off of.

    Returns:
        tuple: (wiring estimate, the best move), or (infinity, None) if no spot is free.
    """
    bbox = part.place_bbox * tx
    free = np.ones(len(offsets), dtype=bool)
    if len(boxes):
        x, y = offsets[:, 0, None], offsets[:, 1, None]
        free = ~(
            (bbox.min.x + x < boxes[:, 2])
            & (bbox.max.x + x > boxes[:, 0])
            & (bbox.min.y + y < boxes[:, 3])
            & (bbox.max.y + y > boxes[:, 1])
        ).any(axis=1)
    if not free.any():
        return float("inf"), None

    cost = np.zeros(len(offsets))
    for pin, others in partners.items():
        pt = pin.pt * tx
        out = _unit(pin_exit(pin, tx))
        # Vectors from the pin (at each spot) to each of the pins it's wired to.
        dx = others[None, :, 0] - (pt.x + offsets[:, 0, None])
        dy = others[None, :, 1] - (pt.y + offsets[:, 1, None])
        wire = np.abs(dx) + np.abs(dy)
        wire += SPREAD_BEND_COST * ((np.abs(dx) > 1) & (np.abs(dy) > 1))
        wire += SPREAD_UTURN_COST * (dx * out.x + dy * out.y < -1)
        cost += wire.min(axis=1)

    # Among equal spots, the offsets' order makes the nearest one win.
    cost[~free] = np.inf
    best = int(np.argmin(cost))
    return float(cost[best]), Vector(float(offsets[best, 0]), float(offsets[best, 1]))


def _turned(part):
    """List a part's transform, then that for each other orientation it's allowed.

    The part is turned about the center of its placement bbox and put back on the grid.
    """
    txs = [part.tx]
    if getattr(part, "orientation_locked", False):
        return txs
    now = linear(part.tx)
    ctr = part.place_bbox.ctr * part.tx
    for lin in best_orientations(part):
        if (lin.a, lin.b, lin.c, lin.d) == (now.a, now.b, now.c, now.d):
            continue
        tx = lin.move(ctr - part.place_bbox.ctr * lin)
        pt = part.pins[0].pt * tx
        txs.append(tx.move(pt.snap(GRID) - pt))
    return txs


def spread_parts(parts, nets):
    """Move seeded parts off of each other and onto the grid.

    Parts are taken biggest first. Each goes to the free spot near its seeded
    position that needs the least wire to the parts already down, favoring
    spots where its wires are straight, and is turned whichever way the
    drafting conventions allow that does that best. So a part that the seed
    left on top of another (a feedback resistor on its opamp) steps to one
    side, while a part that's already clear and level with its neighbor's pin
    stays put. Then each part gets a chance to move again now that it can see
    all the others.

    Args:
        parts (list): Seeded parts (no NetTerminals or satellites).
        nets (list): Nets that will be wired between them.
    """
    net_set = set(nets)
    wired = {part: _wired_pins(part, net_set) for part in parts}
    order = sorted(parts, key=lambda part: (-len(wired[part]), -part.place_bbox.area))

    # Moves to try from a part's position, nearest first.
    steps = range(-SPREAD_MAX_STEPS, SPREAD_MAX_STEPS + 1)
    offsets = np.array(
        sorted(
            ((i * GRID, j * GRID) for i in steps for j in steps),
            key=lambda v: (abs(v[0]) + abs(v[1]), v[1] < 0, v[0] < 0, abs(v[0])),
        ),
        dtype=float,
    )

    placed = {}  # Part -> its placement bbox as (min x, min y, max x, max y).

    def move(part):
        """Move and turn a part to its best spot among the parts that are placed."""
        partners = {}
        for pin in wired[part]:
            pts = [
                other.pt * other.part.tx
                for other in pin.net.pins
                if other.part in placed and other.part is not part and _is_wired(other, net_set)
            ]
            if pts:
                partners[pin] = np.array([(pt.x, pt.y) for pt in pts])
        boxes = np.array([box for other, box in placed.items() if other is not part])
        best_cost = float("inf")
        for tx in _turned(part):
            cost, offset = _best_spot(part, tx, offsets, partners, boxes)
            # The part's current orientation is tried first and wins ties.
            if cost < best_cost:
                best_cost, part.tx = cost, tx.move(offset)
        # (If there was no room nearby, the part stays where it is and the
        # router is left to fail and ask for more space.)
        bbox = part.place_bbox * part.tx
        placed[part] = (bbox.min.x, bbox.min.y, bbox.max.x, bbox.max.y)

    for part in order:
        # Put the part's snap point on the grid; moves of whole grid steps keep it there.
        pt = part.pins[0].pt * part.tx
        part.tx = part.tx.move(pt.snap(GRID) - pt)
        move(part)

    for _ in range(SPREAD_PASSES):
        for part in reversed(order):
            move(part)
