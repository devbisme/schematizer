# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""find_overlaps() (sweep-and-prune) must match a brute-force all-pairs search
exactly, including neighbor order, since the order decides which random offset
each overlap gets during placement."""

import random

import pytest

from schematizer.engine.place import find_overlaps
from schematizer.geometry import BBox, Point


def _brute(parts, bboxes):
    return {
        p: [o for o in parts if o is not p and bboxes[p].intersects(bboxes[o])]
        for p in parts
    }


@pytest.mark.parametrize("seed", range(20))
def test_matches_brute_force(seed):
    rnd = random.Random(seed)
    n = rnd.randint(1, 60)
    parts = [object() for _ in range(n)]
    bboxes = {}
    for p in parts:
        # Coarse integer coords so boxes often share edges exactly (touching, not
        # overlapping) and some have zero width or height.
        x, y = rnd.randint(0, 20) * 50, rnd.randint(0, 20) * 50
        w, h = rnd.randint(0, 6) * 50, rnd.randint(0, 6) * 50
        bboxes[p] = BBox(Point(x, y), Point(x + w, y + h))
    assert find_overlaps(parts, bboxes) == _brute(parts, bboxes)


def test_touching_boxes_do_not_overlap():
    a, b = object(), object()
    bboxes = {a: BBox(Point(0, 0), Point(100, 100)), b: BBox(Point(100, 0), Point(200, 100))}
    assert find_overlaps([a, b], bboxes) == {a: [], b: []}


def test_identical_boxes_overlap():
    a, b = object(), object()
    bboxes = {a: BBox(Point(0, 0), Point(100, 100)), b: BBox(Point(0, 0), Point(100, 100))}
    assert find_overlaps([a, b], bboxes) == {a: [b], b: [a]}
