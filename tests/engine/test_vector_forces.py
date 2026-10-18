# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""The numpy force computation (VectorForces) in push_and_pull must place parts
exactly where the scalar force functions do, for every option it handles."""

import functools
import random
from collections import defaultdict

import pytest

from schematizer.engine import place
from schematizer.engine.rng import seed as rng_seed
from schematizer.geometry import BBox, Point, Tx


class _Obj:
    """Stand-in part/pin, hashable by identity like the real ones."""

    def __init__(self, **attrs):
        self.__dict__.update(attrs)

    def __repr__(self):
        return f"<_Obj {id(self):#x}>"


def _part(rnd):
    tx = Tx()
    for _ in range(rnd.randrange(4)):
        tx = tx.rot_90cw()
    if rnd.random() < 0.5:
        tx = tx.flip_x()
    w, h = rnd.randint(1, 8) * 50, rnd.randint(1, 8) * 50
    return _Obj(
        tx=tx.move(Point(rnd.randint(-20, 20) * 50, rnd.randint(-20, 20) * 50)),
        place_bbox=BBox(Point(-w, -h), Point(w, h)),
        anchor_pins=defaultdict(list),
        pull_pins=defaultdict(list),
    )


def _net_layout(seed, n_mobile, n_anchored, n_outside):
    """Mobile and anchored parts, plus parts outside the placement whose pins still pull."""
    rnd = random.Random(seed)
    mobile = [_part(rnd) for _ in range(n_mobile)]
    anchored = [_part(rnd) for _ in range(n_anchored)]
    outside = [_part(rnd) for _ in range(n_outside)]
    everyone = mobile + anchored + outside
    nets = [object() for _ in range(max(2, len(everyone) // 2))]
    pins = []
    for part in everyone:
        for _ in range(rnd.randint(1, 3)):
            pin = _Obj(
                part=part,
                place_pt=Point(rnd.randint(-4, 4) * 50, rnd.randint(-4, 4) * 50),
                net=rnd.choice(nets),
            )
            pins.append(pin)
            part.anchor_pins[pin.net].append(pin)
    for pin in pins:
        for part in everyone:
            if part is not pin.part:
                part.pull_pins[pin.net].append(pin)
    return anchored, mobile


def _similarity_layout(seed, n):
    rnd = random.Random(seed)
    parts = [_part(rnd) for _ in range(n)]
    anchors = []
    for part in parts:
        pin = _Obj(part=part, place_pt=Point(0, rnd.randint(-4, 4) * 50))
        part.anchor_pins = {"similarity": [pin]}
        anchors.append(pin)
    for part in parts:
        part.pull_pins = {"similarity": anchors}
    similarity = defaultdict(lambda: defaultdict(lambda: 0))
    for a in parts:
        for b in parts:
            if a is not b:
                similarity[a][b] = rnd.choice([0, 0.1, 0.5, 1])
    return parts, similarity


def _run(anchored, mobile, force_func, vectorized, monkeypatch, **options):
    """Run push_and_pull from the parts' current positions; return the final txs."""
    start = {id(p): p.tx for p in anchored + mobile}
    monkeypatch.setattr(place, "VECTOR_MIN_PARTS", 0 if vectorized else 10**9)
    rng_seed(1234)
    place.push_and_pull(anchored, mobile, [], force_func, use_push_pull=True, **options)
    result = [(t.a, t.b, t.c, t.d, t.dx, t.dy) for t in (p.tx for p in mobile)]
    for p in anchored + mobile:
        p.tx = start[id(p)]
    return result


@pytest.mark.parametrize("seed", range(6))
@pytest.mark.parametrize(
    "options",
    [
        dict(pt_to_pt_mult=5, pin_normalize=True),
        dict(pt_to_pt_mult=1, pin_normalize=False),
        dict(pt_to_pt_mult=5, pin_normalize=True, net_normalize=True),
    ],
)
@pytest.mark.parametrize("n_anchored", [0, 3])
def test_net_forces_match_scalar(seed, options, n_anchored, monkeypatch):
    anchored, mobile = _net_layout(seed, n_mobile=7, n_anchored=n_anchored, n_outside=2)
    vec = _run(anchored, mobile, place.total_part_force, True, monkeypatch, **options)
    ref = _run(anchored, mobile, place.total_part_force, False, monkeypatch, **options)
    assert vec == ref


@pytest.mark.parametrize("seed", range(6))
def test_similarity_forces_match_scalar(seed, monkeypatch):
    parts, similarity = _similarity_layout(seed, n=9)
    force_func = functools.partial(place.total_similarity_force, similarity=similarity)
    vec = _run([], parts, force_func, True, monkeypatch)
    ref = _run([], parts, force_func, False, monkeypatch)
    assert vec == ref


def test_unknown_force_function_uses_scalar_code():
    parts, _ = _similarity_layout(0, n=6)
    assert place.VectorForces.build(lambda *a, **k: None, parts, parts) is None
