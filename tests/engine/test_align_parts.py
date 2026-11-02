# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""align_parts() nudges parts so connected pins that face each other end up on
the same line (so the router can join them with a straight wire), without
creating overlaps or moving parts it shouldn't."""

import pytest

from schematizer.engine import constants, place
from schematizer.geometry import BBox, Point, Tx

# place() injects GRID etc. into the module when it runs; do the same here.
place.__dict__.update(constants.__dict__)
GRID = constants.GRID


class _Obj:
    """Stand-in part/pin/net, hashable by identity like the real ones."""

    def __init__(self, **attrs):
        self.__dict__.update(attrs)


def _resistor(x, y):
    """Horizontal two-pin part centered at (x, y): pin 1 exits left, pin 2 exits right."""
    part = _Obj(
        tx=Tx().move(Point(x, y)),
        place_bbox=BBox(Point(-150, -50), Point(150, 50)),
    )
    part.pins = [
        _Obj(part=part, pt=Point(-150, 0), orientation="R", stub=False),
        _Obj(part=part, pt=Point(150, 0), orientation="L", stub=False),
    ]
    return part


def _net(*pins):
    return _Obj(pins=list(pins))


def _pin_pt(pin):
    return pin.pt * pin.part.tx


def test_facing_pins_offset_by_a_grid_step_get_aligned():
    r1, r2 = _resistor(0, 0), _resistor(500, GRID)
    net = _net(r1.pins[1], r2.pins[0])
    before = place.alignment_stats([r1, r2], [net])
    assert before["aligned"] == 0 and before["near_miss"] == 2

    place.align_parts([r1], [r2], [net])

    assert _pin_pt(r2.pins[0]).y == _pin_pt(r1.pins[1]).y
    after = place.alignment_stats([r1, r2], [net])
    assert after["aligned"] == 2 and after["near_miss"] == 0


def test_anchored_parts_dont_move():
    r1, r2 = _resistor(0, 0), _resistor(500, GRID)
    net = _net(r1.pins[1], r2.pins[0])
    place.align_parts([r1, r2], [_resistor(5000, 5000)], [net])
    assert _pin_pt(r2.pins[0]) == Point(350, GRID)


def test_pins_facing_away_are_left_alone():
    # r2 is left of r1, so r1's right pin and r2's left pin point away from each other.
    r1, r2 = _resistor(0, 0), _resistor(-500, GRID)
    net = _net(r1.pins[1], r2.pins[0])
    place.align_parts([r1], [r2], [net])
    assert _pin_pt(r2.pins[0]) == Point(-650, GRID)


def test_offsets_beyond_the_limit_are_left_alone():
    r1, r2 = _resistor(0, 0), _resistor(500, 4 * GRID)
    net = _net(r1.pins[1], r2.pins[0])
    place.align_parts([r1], [r2], [net], align_max_offset=3)
    assert _pin_pt(r2.pins[0]).y == 4 * GRID


def test_alignment_can_be_disabled():
    r1, r2 = _resistor(0, 0), _resistor(500, GRID)
    net = _net(r1.pins[1], r2.pins[0])
    place.align_parts([r1], [r2], [net], align_parts=False)
    assert _pin_pt(r2.pins[0]).y == GRID


def test_moves_that_create_overlaps_are_rejected():
    r1, r2 = _resistor(0, 0), _resistor(500, 2 * GRID)
    # A blocker sitting right where r2 would have to move.
    blocker = _resistor(500, -GRID)
    net = _net(r1.pins[1], r2.pins[0])
    place.align_parts([r1, blocker], [r2], [net])
    assert _pin_pt(r2.pins[0]).y == 2 * GRID


def test_stub_pins_are_ignored():
    r1, r2 = _resistor(0, 0), _resistor(500, GRID)
    r2.pins[0].stub = True  # Drawn as a label, so there's no wire to straighten.
    net = _net(r1.pins[1], r2.pins[0])
    place.align_parts([r1], [r2], [net])
    assert _pin_pt(r2.pins[0]).y == GRID


@pytest.mark.parametrize("rotation", [0, 1, 2, 3])
def test_alignment_follows_part_rotation(rotation):
    """Pin exit directions are taken after the part's transform."""
    tx = Tx()
    for _ in range(rotation):
        tx = tx.rot_90cw()
    r1, r2 = _resistor(0, 0), _resistor(500, GRID)
    for r in (r1, r2):
        r.tx = r.tx * tx  # Rotate about the origin after positioning.
    net = _net(r1.pins[1], r2.pins[0])
    before = place.alignment_stats([r1, r2], [net])
    place.align_parts([r1], [r2], [net])
    after = place.alignment_stats([r1, r2], [net])
    # Rotating both parts together about the origin keeps the pins facing.
    assert before["near_miss"] == 2
    assert after["aligned"] == 2
