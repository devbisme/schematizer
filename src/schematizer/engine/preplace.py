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
* Spread: seeded parts that landed on each other are moved apart.

Once the parts are down, each net label is put beside a pin of its net.

Parts with no wires at all (a bank of decoupling caps whose pins all go to
power symbols) are put in tidy rows, grouped by the rails they're on.
"""

import heapq
from collections import defaultdict
from functools import lru_cache
from itertools import combinations

import numpy as np

from ..geometry import BBox, Point, Tx, Vector
from .constants import GRID
from .conventions import (
    best_orientations,
    is_ground_net_name,
    linear,
    orientation_cost,
    orientations,
    pin_exit,
)

__all__ = [
    "find_satellites",
    "attach_satellites",
    "release_satellites",
    "place_terminals",
    "seed_placement",
    "spread_parts",
    "row_floating_parts",
]

# Gap between parts in the rows of parts with no wires, and how many times
# wider than tall the rows are made.
FLOAT_GAP = 2 * GRID
FLOAT_ASPECT = 4

# Size (in mils) of the reference and value text drawn beside a part: the gap
# between it and the part, the width of a character, and the line pitch.
FIELD_GAP = 35
FIELD_CHAR_W = 30
FIELD_PITCH = 50

# Gap left between the placement boxes of two parts joined by a straight wire.
SEED_GAP = 2 * GRID
# Most pins a net can have and still have every pair of them pull together.
SEED_MAX_CLIQUE = 10

# How far (in grid steps) a part can be moved at a time to get it off of other
# parts, and the wire length a bend is worth when choosing where it goes.
SPREAD_MAX_STEPS = 20
# If that isn't enough to find room for a part, how far out the search for some
# goes (in grid steps). It's far enough to get past anything on a sheet.
SPREAD_MAX_RADIUS = 1000
SPREAD_BEND_COST = 4 * GRID
# Wire length it's worth for a wire not to have to double back behind its pin.
SPREAD_UTURN_COST = 8 * GRID
# Times each part gets to move again after all the parts are down.
SPREAD_PASSES = 2

# Search limits (in grid steps) for a spot beside a pin. They're wide enough
# for the satellites on a row of IC pins to be staggered one beyond the other
# when they're too tall to sit in a column.
SAT_MAX_OUT = 40
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

# Space kept between a satellite and other parts so the router can get a wire
# through. It isn't widened when routing fails and is tried again with more
# room around the parts: wider gaps here were measured to route worse.
SAT_CLEARANCE = GRID

# Space a wire needs between it and a part, or the router has no track to put it on.
WIRE_CLEARANCE = GRID / 2

# Extra space left around a part that's put in open space because there was
# no room for it beside a pin.
FALLBACK_CLEARANCE = 2 * GRID

# Length of the corridor kept clear where a wire leaves a part, so a satellite
# hanging from one pin doesn't cut across the wire from another pin on the same side.
SAT_EXIT_CLEARANCE = 12 * GRID

# The same for net labels, which only need to stay off the ends of other pins.
TERMINAL_EXIT_CLEARANCE = 3 * GRID

# Space left between a net label and the pin it's beside when other pins on
# the net have to be wired to that pin too. With less, the router has no track
# between them for those wires to come in on.
TERMINAL_JOIN_GAP = 2 * GRID


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


@lru_cache(maxsize=None)
def _spot_offsets(out, exit_vec):
    """List where a part's pin could go relative to the pin it's wired to.

    Args:
        out (tuple): Direction (x, y) a wire leaves the pin it's wired to.
        exit_vec (tuple): Direction a wire leaves the part's own pin.

    Returns:
        tuple: (wire cost, offset is leftward, offset is downward, offset in
            grid steps, corner of the wire in grid steps or None), cheapest first.
    """
    out, exit_vec = Vector(*out), Vector(*exit_vec)
    side = Vector(-out.y, out.x)
    facing = exit_vec.x * out.x + exit_vec.y * out.y
    if facing < 0:
        # Pins face each other: a straight wire, or a jog if stepped sideways.
        offsets = [
            (k + SAT_SIDE_COST * abs(j), out * k + side * j, None)
            for k in range(2, SAT_MAX_OUT + 1)
            for j in range(-SAT_MAX_SIDE, SAT_MAX_SIDE + 1)
        ]
    elif facing == 0:
        # Pins at right angles: the wire goes out, then turns into the part.
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
    # Break ties the same way every time: positive side first.
    keyed = [
        (cost, offset.x < 0, offset.y < 0, n, offset, corner)
        for n, (cost, offset, corner) in enumerate(offsets)
    ]
    keyed.sort(key=lambda entry: entry[:4])
    return tuple((cost, left, down, offset, corner) for cost, left, down, _, offset, corner in keyed)


def _spots(part, pin, host_pt, out, lins):
    """List the places a part could go beside the pin that one of its pins is wired to.

    Orientations that break the drafting conventions cost extra, so a
    decoupling cap stays upright with its ground pin down, even with a bend in
    its wire, unless that would push it far from its pin.

    Args:
        part (Part): The part to place.
        pin (Pin): Its wired pin.
        host_pt (Point): Position of the pin it's wired to.
        out (Vector): Direction a wire leaves that pin.
        lins (list): Orientations (rotation/flip transforms) the part can have.

    Yields:
        tuple: (transform for the part, corner of its wire or None), best first.
    """
    least_broken = min(orientation_cost(part, lin) for lin in lins)

    def spots_for(turn, lin):
        """Spots for one orientation, cheapest first."""
        broken = (orientation_cost(part, lin) - least_broken) * SAT_CONVENTION_COST
        exit_vec = _unit(pin_exit(pin, lin))
        offsets = _spot_offsets((out.x, out.y), (exit_vec.x, exit_vec.y))
        for n, (cost, left, down, offset, corner) in enumerate(offsets):
            yield (cost + broken, left, down, turn, n), lin, offset, corner

    # Only a few spots get looked at before one is free, so merge the
    # orientations' lists and work out transforms as they're asked for.
    streams = [spots_for(turn, lin) for turn, lin in enumerate(lins)]
    for _, lin, offset, corner in heapq.merge(*streams, key=lambda spot: spot[0]):
        shift = host_pt + offset * GRID - pin.pt * lin
        yield lin.move(shift), None if corner is None else host_pt + corner * GRID


class _Keepouts:
    """The boxes that parts placed beside pins, and their wires, have to keep out of.

    Each box has an owner (a part, or a pin for the wires from it) that's
    allowed to touch it. Parts keep their distance from other parts so the
    router can get wires between them. A wire can run closer to a part than that.
    """

    def __init__(self, clearance, exit_clearance):
        self.clearance = clearance
        self.exit_clearance = exit_clearance
        self.for_parts = []  # (owner, box) that a part can't overlap.
        self.for_wires = []  # (owner, box) that a wire can't cross.

    def add_part(self, part, box):
        self.for_parts.append((part, _grow(box, self.clearance)))
        self.for_wires.append((part, _grow(box, WIRE_CLEARANCE)))

    def add_wire(self, pin, box):
        self.for_parts.append((pin, box))
        self.for_wires.append((pin, box))

    def add_exit(self, pin, tx):
        """Keep clear of where a wire leaves a pin and passes its part's labels.

        Args:
            pin (Pin): The pin.
            tx (Tx): Transform of the pin's part.
        """
        pt = pin.pt * tx
        out = _unit(pin_exit(pin, tx))
        box = pin.part.lbl_bbox * tx
        reach = max(
            (box.max.x - pt.x) * out.x,
            (box.min.x - pt.x) * out.x,
            (box.max.y - pt.y) * out.y,
            (box.min.y - pt.y) * out.y,
        )
        tip = pt + out * (reach + self.exit_clearance)
        self.add_wire(pin, _grow(BBox(pt, tip), GRID / 2))

    def find_spot(self, part, pin, host_pin, host_tx, lins, same_net=(), min_gap=0):
        """Find the best free spot for a part beside the pin it's wired to, and take it.

        Args:
            part (Part): The part to place.
            pin (Pin): Its wired pin.
            host_pin (Pin): The pin it's wired to.
            host_tx (Tx): Transform of the host pin's part.
            lins (list): Orientations the part can have.
            same_net (collection): Other pins whose wires the part can share
                the way with, because they're on its net.
            min_gap (float): Space to leave between the pin and the part, so
                the router has a track there for other wires to join this one.

        Returns:
            Tx | None: Transform for the part, or None if every spot is taken.
        """
        host_pt = host_pin.pt * host_tx
        out = _unit(pin_exit(host_pin, host_tx))
        for tx, corner in _spots(part, pin, host_pt, out, lins):
            box = part.lbl_bbox * tx
            if min_gap and _gap_ahead(host_pt, out, box) < min_gap:
                continue
            wire_pts = [host_pt, pin.pt * tx] + ([] if corner is None else [corner])
            wire = _grow(BBox(*wire_pts), 1)
            # The wire has to touch the part it comes from and can share the
            # way with other wires from the same pin, but nothing else.
            if not any(
                box.intersects(b)
                for owner, b in self.for_parts
                if owner is not host_pin and owner not in same_net
            ) and not any(
                wire.intersects(b)
                for owner, b in self.for_wires
                if owner is not host_pin
                and owner is not host_pin.part
                and owner not in same_net
            ):
                self.add_part(part, box)
                self.add_wire(host_pin, wire)
                return tx
        return None


    def find_room(self, part, pin, near, lins):
        """Find the nearest free spot for a part around a point, and take it.

        This is for a part there's no spot for beside a pin. It only finds
        open space; the part's wire is left for the router to work out.

        Args:
            part (Part): The part to place.
            pin (Pin): Its wired pin, which goes on the grid point found.
            near (Point): Where to search outward from.
            lins (list): Orientations the part can have.

        Returns:
            Tx | None: Transform for the part, or None if no room was found.
        """
        near = near.snap(GRID)
        for radius in range(1, SPREAD_MAX_RADIUS):
            for dx, dy in _ring(radius):
                for lin in lins:
                    tx = lin.move(near + Vector(float(dx), float(dy)) - pin.pt * lin)
                    box = part.lbl_bbox * tx
                    # Leave extra room all around, and stay out of the way of
                    # the wires leaving pins, so the router can get to it.
                    roomy = _grow(box, FALLBACK_CLEARANCE)
                    if not any(
                        roomy.intersects(b) for _, b in self.for_parts + self.for_wires
                    ):
                        self.add_part(part, box)
                        return tx
        return None


def _gap_ahead(pt, out, box):
    """Distance from a point to a box, measured in the direction out from the point."""
    return min((corner - pt).x * out.x + (corner - pt).y * out.y for corner in (box.min, box.max))


def attach_satellites(host, satellites, nets):
    """Fix each satellite beside the pin it's wired to and grow the host to cover them.

    Each satellite gets a ``rel_tx`` that maps it into its host's coordinates,
    so after placement its transform is ``rel_tx * host.tx``. The satellite's
    wired pin sits a whole number of grid steps from the host pin, so it stays
    on grid when the host does.

    Args:
        host (Part): Part that will carry the satellites.
        satellites (list): Its entry from find_satellites().
        nets (list): Nets that will be wired between parts.

    Returns:
        list: The entries of the satellites that were attached. One that there
            was no room for beside its pin is left out (along with any wired to
            it), to be placed like any other part.
    """
    nets = set(nets)
    keepouts = _Keepouts(SAT_CLEARANCE, SAT_EXIT_CLEARANCE)

    # Everything is laid out around the host as it's oriented now, with the
    # host's origin at (0, 0).
    host_lin = linear(host.tx)
    to_host = _inverse(host_lin)
    txs = {host: host_lin}

    keepouts.add_part(host, host.lbl_bbox * host_lin)
    for pin in _wired_pins(host, nets):
        keepouts.add_exit(pin, host_lin)

    host.preplace_bbox = host.place_bbox
    bbox = BBox(host.place_bbox.min, host.place_bbox.max)
    host.riders = []
    attached = []
    for sat, sat_pin, host_pin in satellites:
        if host_pin.part not in txs:
            continue  # What it's wired to didn't fit, so it has nothing to sit beside.
        tx = keepouts.find_spot(
            sat, sat_pin, host_pin, txs[host_pin.part], orientations(sat.tx)
        )
        if tx is None:
            continue
        txs[sat] = tx
        sat.rel_tx = tx * to_host
        host.riders.append(sat)
        attached.append((sat, sat_pin, host_pin))
        for pin in _wired_pins(sat, nets):
            if pin is not sat_pin:
                keepouts.add_exit(pin, tx)
        bbox.add(sat.place_bbox * sat.rel_tx)
    host.place_bbox = bbox
    return attached


# The ways a net label can be turned: one for each direction its wire can leave it.
_TERMINAL_LINS = [Tx(), Tx().flip_x(), Tx().rot_90cw(), Tx().rot_90cw().rot_90cw().rot_90cw()]


def place_terminals(terminals, parts, nets):
    """Put each net label beside one of the pins of its net.

    The label goes straight out from a pin if there's room, so its wire is
    short and doesn't have to find a way to some open space at the edge of the
    sheet. Pins nearer the outside of the placed parts are tried first. If
    there's no room beside any of them, it goes in the nearest open space.

    Args:
        terminals (list): NetTerminals to place.
        parts (list): Placed parts (no NetTerminals).
        nets (list): Nets that will be wired between parts.

    Returns:
        list: The NetTerminals that couldn't be placed (their nets have no
            wired pins on the parts). They still need placing.
    """
    if not terminals:
        return []

    net_set = set(nets)
    part_set = set(parts)
    keepouts = _Keepouts(SAT_CLEARANCE, TERMINAL_EXIT_CLEARANCE)
    area = BBox()
    for part in parts:
        box = part.lbl_bbox * part.tx
        keepouts.add_part(part, box)
        area.add(box)
        for pin in _wired_pins(part, net_set):
            keepouts.add_exit(pin, part.tx)

    def inset(pin):
        """How far inside the placed parts a wire leaving a pin starts."""
        pt = pin.pt * pin.part.tx
        out = _unit(pin_exit(pin, pin.part.tx))
        return min(
            (area.max.x - pt.x) if out.x > 0 else float("inf"),
            (pt.x - area.min.x) if out.x < 0 else float("inf"),
            (area.max.y - pt.y) if out.y > 0 else float("inf"),
            (pt.y - area.min.y) if out.y < 0 else float("inf"),
        )

    unplaced = []
    for terminal in terminals:
        pin = terminal.pins[0]
        if getattr(terminal, "orientation_locked", False):
            lins = [linear(terminal.tx)]  # An input or output label keeps its direction.
        else:
            lins = _TERMINAL_LINS
        host_pins = [
            p for p in pin.net.pins if p.part in part_set and _is_wired(p, net_set)
        ]
        host_pins.sort(key=inset)
        tx = None
        for host_pin in host_pins:
            tx = keepouts.find_spot(
                terminal,
                pin,
                host_pin,
                host_pin.part.tx,
                lins,
                same_net=host_pins,
                min_gap=TERMINAL_JOIN_GAP if len(host_pins) > 1 else 0,
            )
            if tx is not None:
                break
        if tx is None and host_pins:
            # No room beside any pin, so use the nearest open space to one.
            tx = keepouts.find_room(
                terminal, pin, host_pins[0].pt * host_pins[0].part.tx, lins
            )
        if tx is None:
            unplaced.append(terminal)
        else:
            terminal.tx = tx
    return unplaced


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
        if len(pins) > SEED_MAX_CLIQUE:
            # Pairing every pin with every other gets out of hand for a big
            # net, so just pair each with the next.
            pairs = zip(pins, pins[1:])
            weight = 1.0
        else:
            pairs = combinations(pins, 2)
        for pin_a, pin_b in pairs:
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


def _ring(radius):
    """Moves (as an array) to the grid points on a square this many grid steps out."""
    steps = range(-radius, radius + 1)
    edge = [(i, j) for i in steps for j in (-radius, radius)]
    edge += [(i, j) for i in (-radius, radius) for j in steps[1:-1]]
    edge.sort(key=lambda v: (abs(v[0]) + abs(v[1]), v[1] < 0, v[0] < 0, abs(v[0])))
    return np.array(edge, dtype=float) * GRID


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
        if best_cost == float("inf"):
            # No room nearby, so look farther and farther out until there is.
            for radius in range(SPREAD_MAX_STEPS + 1, SPREAD_MAX_RADIUS, 2):
                _, offset = _best_spot(part, part.tx, _ring(radius), partners, boxes)
                if offset is not None:
                    part.tx = part.tx.move(offset)
                    break
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


def row_floating_parts(parts):
    """Put parts that have no wires in rows, grouped by the nets on their pins.

    These are mostly decoupling caps whose pins all go to power symbols, and a
    person draws them as a row of caps with the ones on the same rail side by
    side. The rows are kept a few times wider than they are tall.

    Args:
        parts (list): Parts with no wired pins. Their orientations are kept.
    """

    def rails(part):
        names = {pin.net.name for pin in part.pins if pin.net is not None}
        return sorted(name for name in names if not is_ground_net_name(name))

    def key(part):
        return (rails(part), len(part.pins), str(getattr(part, "value", "")), part.ref)

    def with_text(part):
        """Box of the part and its labels, plus room for its reference and value.

        The text goes on a side with no pins: to the right of a part with
        pins only on its top and bottom, otherwise above and below it.
        """
        lin = linear(part.tx)
        box = part.lbl_bbox * lin
        chars = max(len(str(getattr(part, "ref", ""))), len(str(getattr(part, "value", ""))))
        if all(round(pin_exit(pin, lin).x) == 0 for pin in part.pins):
            return BBox(box.min, box.max + Vector(FIELD_GAP + chars * FIELD_CHAR_W, 0))
        return box.resize(Vector(0, FIELD_GAP + 2 * FIELD_PITCH))

    parts = sorted(parts, key=key)
    boxes = [with_text(part) for part in parts]
    area = sum((box.w + FLOAT_GAP) * (box.h + FLOAT_GAP) for box in boxes)
    width = max(max(box.w for box in boxes), (area * FLOAT_ASPECT) ** 0.5)

    x = top = row_height = 0
    for part, box in zip(parts, boxes):
        if x and x + box.w > width:
            # Start a new row below the last one.
            x, top, row_height = 0, top - row_height - FLOAT_GAP, 0
        part.tx = linear(part.tx).move(Vector(x - box.min.x, top - box.max.y))
        pt = part.pins[0].pt * part.tx
        part.tx = part.tx.move(pt.snap(GRID) - pt)
        x += box.w + FLOAT_GAP
        row_height = max(row_height, box.h)
