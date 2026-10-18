# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""The cached/numpy orientation costs (OrientationCosts) used by
adjust_orientations must score every orientation exactly as net_tension_dist
and label_overlap_cost do, and so pick the same orientations."""

import random
from collections import defaultdict

import pytest

from schematizer.engine import place
from schematizer.geometry import BBox, Point, Tx


class _Pin:
    """Stand-in pin, hashable by identity like the real ones."""

    def __init__(self, part, pt, orientation, stub):
        self.part = part
        self.pt = pt
        self.place_pt = Point(pt.x, pt.y)
        self.orientation = orientation
        self.stub = stub

    def is_connected(self):
        return True


class _Part(list):
    """Stand-in part: an iterable of pins with placement geometry."""

    __hash__ = object.__hash__
    __eq__ = object.__eq__

    def __init__(self, rnd, has_bbox=True):
        super().__init__()
        tx = Tx()
        for _ in range(rnd.randrange(4)):
            tx = tx.rot_90cw()
        if rnd.random() < 0.5:
            tx = tx.flip_x()
        self.tx = tx.move(Point(rnd.randint(-10, 10) * 50, rnd.randint(-10, 10) * 50))
        w, h = rnd.randint(1, 4) * 50, rnd.randint(1, 4) * 50
        if has_bbox:
            self.place_bbox = BBox(Point(-w, -h), Point(w, h))
        self.orientation_locked = False
        self.anchor_pins = defaultdict(list)
        self.pull_pins = defaultdict(list)
        for _ in range(rnd.randint(1, 4)):
            pt = Point(rnd.randint(-w // 50, w // 50) * 50, rnd.randint(-h // 50, h // 50) * 50)
            self.append(_Pin(self, pt, rnd.choice("UDLR"), rnd.random() < 0.8))


def _layout(seed, n_parts, floating):
    """Parts with anchor/pull pins set up like add_anchor_pull_pins() does:
    either connected by nets or, if floating, all pulled by each other
    (including by their own anchor pin)."""
    rnd = random.Random(seed)
    parts = [_Part(rnd) for _ in range(n_parts)]
    if floating:
        all_pull_pins = []
        for part in parts:
            pin = max(part, key=lambda p: p.pt.y)
            part.anchor_pins["similarity"] = [pin]
            part.pull_pins["similarity"] = all_pull_pins
            all_pull_pins.append(pin)
    else:
        nets = [object() for _ in range(max(2, n_parts // 2))]
        net_pins = defaultdict(list)
        for part in parts:
            for pin in part:
                net_pins[rnd.choice(nets)].append(pin)
        for net, pins in net_pins.items():
            for pin in pins:
                pin.part.anchor_pins[net].append(pin)
                for other in dict.fromkeys(p.part for p in pins):
                    if other is not pin.part:
                        other.pull_pins[net].append(pin)
    return parts


def _orientations(part):
    """Step the part through its 8 orientations, as find_best_orientation does."""
    ctr = (part.place_bbox * part.tx).ctr
    start = part.tx
    for _ in range(2):
        for _ in range(4):
            yield
            part.tx = part.tx.move(-ctr).rot_90cw().move(ctr)
        part.tx = part.tx.move(-ctr).flip_x().move(ctr)
    part.tx = start


@pytest.fixture(params=["python", "numpy"])
def path(request, monkeypatch):
    """Run the cache with its plain-Python loops or with numpy."""
    if request.param == "numpy":
        monkeypatch.setattr(place, "ORIENT_NUMPY_MIN_PINS", 1)
        monkeypatch.setattr(place, "ORIENT_NUMPY_MIN_BOXES", 1)
    else:
        monkeypatch.setattr(place, "ORIENT_NUMPY_MIN_PINS", 10**9)
        monkeypatch.setattr(place, "ORIENT_NUMPY_MIN_BOXES", 10**9)
    return request.param


@pytest.mark.parametrize("floating", [False, True])
@pytest.mark.parametrize("seed", range(6))
def test_tension_matches_net_tension_dist(seed, floating, path):
    parts = _layout(seed, 12, floating)
    costs = place.OrientationCosts(parts)
    for part in parts:
        costs.refresh()
        for _ in _orientations(part):
            assert costs.tension(part) == place.net_tension_dist(part)
    # Move parts and check the refreshed cache still agrees.
    rnd = random.Random(seed)
    for part in rnd.sample(parts, 4):
        part.tx = part.tx.move(Point(rnd.randint(-5, 5) * 50, 0)).rot_90cw()
    costs.refresh()
    for part in parts:
        assert costs.tension(part) == place.net_tension_dist(part)


@pytest.mark.parametrize("seed", range(6))
def test_label_cost_matches_scalar_neighbors(seed, path):
    rnd = random.Random(seed)
    parts = _layout(seed, 12, floating=True)
    # Neighbors may include parts with no placement bbox, and duplicates.
    neighbors = parts + [_Part(rnd, has_bbox=False), parts[0], parts[3]]
    costs = place.OrientationCosts(parts, neighbors)
    for part in parts:
        costs.refresh()
        for _ in _orientations(part):
            assert place.label_overlap_cost(
                part, _label_neighbors=costs
            ) == place.label_overlap_cost(part, _label_neighbors=neighbors)


class _ScalarCosts(place.OrientationCosts):
    """Reference costs computed by the original scalar code."""

    def tension(self, part):
        return place.net_tension_dist(part)

    def neighbor_hits(self, part, label_boxes):
        hits = 0
        for other in self.neighbors:
            if other is part:
                continue
            try:
                other_bbox = other.place_bbox * other.tx
            except AttributeError:
                continue
            hits += sum(box.intersects(other_bbox) for box in label_boxes)
        return hits


@pytest.mark.parametrize("floating", [False, True])
@pytest.mark.parametrize("seed", range(4))
def test_adjust_orientations_unchanged(seed, floating, path, monkeypatch):
    """Whole orientation search picks the same orientations as the scalar costs."""
    results = []
    for costs_cls in (place.OrientationCosts, _ScalarCosts):
        monkeypatch.setattr(place, "OrientationCosts", costs_cls)
        parts = _layout(seed, 10, floating)
        place.adjust_orientations(parts, _label_aware_scope=floating)
        results.append([(p.tx.a, p.tx.b, p.tx.c, p.tx.d, p.tx.dx, p.tx.dy) for p in parts])
    assert results[0] == results[1]
