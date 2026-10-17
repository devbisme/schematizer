# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""The per-iteration pin-point cache (PinPoints) must not change the placement
forces: computing them with a cache shared across all parts has to give exactly
the same result as computing every pin point afresh."""

import random
from collections import defaultdict
import pytest

from schematizer.engine.place import PinPoints, net_force_dist, similarity_force
from schematizer.geometry import Point, Tx


class _Obj:
    """Stand-in part/pin. Hashable by identity like the real ones (a SimpleNamespace
    isn't), with a short repr since parts and pins reference each other."""

    def __init__(self, **attrs):
        self.__dict__.update(attrs)

    def __repr__(self):
        return f"<_Obj {id(self):#x}>"


def _layout(seed, n_parts=8, n_nets=6):
    """Parts with pins scattered on random nets, each part rotated/flipped/moved."""
    rnd = random.Random(seed)
    parts = []
    for _ in range(n_parts):
        tx = Tx()
        for _ in range(rnd.randrange(4)):
            tx = tx.rot_90cw()
        if rnd.random() < 0.5:
            tx = tx.flip_x()
        part = _Obj(
            tx=tx.move(Point(rnd.uniform(-1000, 1000), rnd.uniform(-1000, 1000))),
            anchor_pins=defaultdict(list),
            pull_pins=defaultdict(list),
        )
        parts.append(part)
    nets = [object() for _ in range(n_nets)]
    pins = []
    for part in parts:
        for _ in range(rnd.randint(1, 4)):
            pin = _Obj(
                part=part,
                place_pt=Point(rnd.uniform(-200, 200), rnd.uniform(-200, 200)),
                net=rnd.choice(nets),
            )
            pins.append(pin)
            part.anchor_pins[pin.net].append(pin)
    for pin in pins:
        for part in parts:
            if part is not pin.part:
                part.pull_pins[pin.net].append(pin)
    return parts


@pytest.mark.parametrize("seed", range(10))
def test_net_force_same_with_shared_cache(seed):
    parts = _layout(seed)
    opts = dict(pt_to_pt_mult=5, pin_normalize=True)
    cache = PinPoints()
    for part in parts:
        cached = net_force_dist(part, pin_pts=cache, **opts)
        fresh = net_force_dist(part, **opts)
        assert (cached.x, cached.y) == (fresh.x, fresh.y)


@pytest.mark.parametrize("seed", range(10))
def test_similarity_force_same_with_shared_cache(seed):
    rnd = random.Random(seed)
    parts = _layout(seed)
    # Floating-part style: one anchor pin per part, every anchor pulls on every part.
    anchors = []
    for part in parts:
        pin = _Obj(part=part, place_pt=Point(rnd.uniform(-200, 200), 0))
        part.anchor_pins = {"similarity": [pin]}
        anchors.append(pin)
    for part in parts:
        part.pull_pins = {"similarity": anchors}
    similarity = defaultdict(lambda: defaultdict(lambda: 0))
    for a in parts:
        for b in parts:
            if a is not b:
                similarity[a][b] = rnd.random()
    cache = PinPoints()
    for part in parts:
        cached = similarity_force(part, parts, similarity, pin_pts=cache)
        fresh = similarity_force(part, parts, similarity)
        assert (cached.x, cached.y) == (fresh.x, fresh.y)


def test_cache_computes_each_pin_once():
    part = _Obj(tx=Tx().move(Point(10, 20)))
    pin = _Obj(part=part, place_pt=Point(1, 2))
    cache = PinPoints()
    first = cache[pin]
    assert (first.x, first.y) == (11, 22)
    assert cache[pin] is first
