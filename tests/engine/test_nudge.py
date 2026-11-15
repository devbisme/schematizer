"""Parts are nudged after routing to straighten wires that step sideways."""

import json
import os
from collections import defaultdict

import pytest

from schematizer import render
from schematizer.engine import gen_schematic, nudge
from schematizer.engine.nudge import find_staircases, wiring_cost
from schematizer.geometry import BBox, Point, Segment, Tx

DATA = os.path.join(os.path.dirname(__file__), "..", "data")


def _load(case):
    with open(os.path.join(DATA, f"{case}.json")) as f:
        return json.load(f)


class _Obj:
    """A stand-in for a part, pin, net or node: attributes, and usable as a dict key."""

    def __init__(self, **attrs):
        self.__dict__.update(attrs)


def _seg(x1, y1, x2, y2):
    return Segment(Point(x1, y1), Point(x2, y2))


def _part(ref, x, y, net, others=()):
    """A part at (x, y) with a pin at its origin on `net`, and more pins on `others`."""
    part = _Obj(ref=ref, tx=Tx(dx=x, dy=y), pins=[])
    part.bbox = part.lbl_bbox = BBox(Point(-20, -20), Point(20, 20))
    for i, pin_net in enumerate((net, *others)):
        part.pins.append(_Obj(pt=Point(0, 50 * i), net=pin_net, part=part))
    return part


def _node(parts, wires, flattened=False):
    node = _Obj(parts=parts, children={}, flattened=flattened, junctions=defaultdict(list))
    node.wires = defaultdict(list, wires)
    return node


NET = _Obj(name="N")


def _staircase_node(**kwargs):
    """R1's pin at (0, 0) wired to R2's at (400, 50) by a wire that steps down 50 at x=200."""
    r1, r2 = _part("R1", 0, 0, NET), _part("R2", 400, 50, NET)
    wires = {NET: [_seg(0, 0, 200, 0), _seg(200, 0, 200, 50), _seg(200, 50, 400, 50)]}
    return _node([r1, r2], wires, **kwargs), r1, r2


class TestFindStaircases:
    def test_finds_a_step_between_two_pins(self):
        node, _, _ = _staircase_node()
        [(net, near, start, end, far)] = find_staircases(node)
        assert net is NET
        assert {near, far} == {Point(0, 0), Point(400, 50)}
        assert {start, end} == {Point(200, 0), Point(200, 50)}

    def test_a_straight_wire_or_single_bend_is_not_one(self):
        r1, r2 = _part("R1", 0, 0, NET), _part("R2", 400, 100, NET)
        assert find_staircases(_node([r1, r2], {NET: [_seg(0, 0, 400, 0)]})) == []
        bend = {NET: [_seg(0, 0, 400, 0), _seg(400, 0, 400, 100)]}
        assert find_staircases(_node([r1, r2], bend)) == []

    def test_a_u_shape_is_not_one(self):
        r1, r2 = _part("R1", 0, 0, NET), _part("R2", 0, 50, NET)
        wires = {NET: [_seg(0, 0, 200, 0), _seg(200, 0, 200, 50), _seg(0, 50, 200, 50)]}
        assert find_staircases(_node([r1, r2], wires)) == []

    def test_a_long_step_is_left_alone(self):
        r1, r2 = _part("R1", 0, 0, NET), _part("R2", 400, 500, NET)
        wires = {NET: [_seg(0, 0, 200, 0), _seg(200, 0, 200, 500), _seg(200, 500, 400, 500)]}
        assert find_staircases(_node([r1, r2], wires)) == []


def test_wiring_cost_counts_bends_and_length():
    node, _, _ = _staircase_node()
    assert wiring_cost(node) == (2, 450)


def test_wiring_cost_counts_a_crossing_like_a_bend():
    other = _Obj(name="OTHER")
    parts = [_part("R1", 0, 0, NET), _part("R2", 400, 0, NET)]
    crossed = _node(parts, {NET: [_seg(0, 0, 400, 0)], other: [_seg(200, -100, 200, 100)]})
    apart = _node(parts, {NET: [_seg(0, 0, 400, 0)], other: [_seg(500, -100, 500, 100)]})
    assert wiring_cost(crossed) == (1, 600)
    assert wiring_cost(apart) == (0, 600)


class TestPlanAndApply:
    def test_moves_one_part_by_the_step(self):
        node, r1, r2 = _staircase_node()
        moves, wired = nudge._plan_moves(node, find_staircases(node))
        [(part, vec)] = moves.items()
        # Either part would do; each has one wired pin. The first by name moves.
        assert part is r1 and (vec.x, vec.y) == (0, 50)
        assert wired[r1] == wired[r2] == 1

    def test_moves_the_part_with_fewer_wired_pins(self):
        other = _Obj(name="M")
        r1, r2 = _part("R1", 0, 0, NET, others=[other]), _part("R2", 400, 50, NET)
        wires = {
            NET: [_seg(0, 0, 200, 0), _seg(200, 0, 200, 50), _seg(200, 50, 400, 50)],
            other: [_seg(-300, 50, 0, 50)],
        }
        node = _node([r1, r2], wires)
        moves, _ = nudge._plan_moves(node, find_staircases(node))
        [(part, vec)] = moves.items()
        assert part is r2 and (vec.x, vec.y) == (0, -50)

    def test_a_move_onto_another_part_is_skipped(self):
        node, r1, _ = _staircase_node()
        node.parts.append(_part("C1", 0, 60, _Obj(name="X")))
        moves, _ = nudge._plan_moves(node, find_staircases(node))
        assert nudge._apply_moves(node, {r1: moves[r1]}) == []
        assert r1.tx.origin == Point(0, 0)

    def test_a_flattened_node_keeps_its_parts_inside_its_space(self):
        node, r1, _ = _staircase_node(flattened=True)
        moves, _ = nudge._plan_moves(node, find_staircases(node))
        assert nudge._apply_moves(node, moves) == [r1]  # Down 50 stays inside.
        from schematizer.geometry import Vector

        assert nudge._apply_moves(node, {r1: Vector(0, -500)}) == []


class TestTryMoves:
    def _routable(self, monkeypatch, new_wires):
        node, r1, r2 = _staircase_node()
        calls = []

        def route_parts(tool=None, **options):
            calls.append(options)
            if isinstance(new_wires, Exception):
                raise new_wires
            node.wires = defaultdict(list, {NET: new_wires})

        node.route_parts = route_parts
        return node, r1, calls

    def test_keeps_a_routing_with_fewer_bends(self, monkeypatch):
        node, r1, calls = self._routable(monkeypatch, [_seg(0, 50, 400, 50)])
        assert nudge.straighten(node) == 1
        assert r1.tx.origin == Point(0, 50)
        assert wiring_cost(node) == (0, 400)
        assert len(calls) == 1

    def test_undoes_a_routing_that_is_no_better(self, monkeypatch):
        worse = [_seg(0, 50, 100, 50), _seg(100, 50, 100, 0), _seg(100, 0, 300, 0),
                 _seg(300, 0, 300, 50), _seg(300, 50, 400, 50)]
        node, r1, _ = self._routable(monkeypatch, worse)
        before = dict(node.wires)
        assert nudge.straighten(node) == 0
        assert r1.tx.origin == Point(0, 0)
        assert dict(node.wires) == before

    def test_undoes_a_move_that_cannot_be_routed(self, monkeypatch):
        from schematizer.engine.route import RoutingFailure

        node, r1, _ = self._routable(monkeypatch, RoutingFailure())
        assert nudge.straighten(node) == 0
        assert r1.tx.origin == Point(0, 0)
        assert wiring_cost(node) == (2, 450)


def _layout_bends(doc, seed, **options):
    """Total (staircases, bends) of a rendered design, read from the routed nodes."""
    seen = {}
    write_output = gen_schematic._write_output

    def spy(circuit, node, *args):
        def walk(n):
            yield n
            for child in n.children.values():
                yield from walk(child)

        nodes = list(walk(node))
        seen["stairs"] = sum(len(find_staircases(n)) for n in nodes)
        seen["bends"] = sum(wiring_cost(n)[0] for n in nodes)
        return write_output(circuit, node, *args)

    gen_schematic._write_output = spy
    try:
        import tempfile

        render(json.loads(json.dumps(doc)), filepath=tempfile.mkdtemp(), seed=seed, **options)
    finally:
        gen_schematic._write_output = write_output
    return seen["stairs"], seen["bends"]


@pytest.mark.python_engine
def test_nudging_never_adds_bends_and_removes_some():
    """Each nudge is kept only if it lowers the bends, so no layout gets worse."""
    doc = _load("opamp_circ")
    better = 0
    for seed in range(1, 7):
        off = _layout_bends(doc, seed, nudge=False)
        on = _layout_bends(doc, seed)
        assert on[1] <= off[1], seed
        better += on[1] < off[1]
    assert better


def test_nudge_false_leaves_parts_alone(monkeypatch, tmp_path):
    monkeypatch.setattr(nudge, "straighten", lambda *a, **k: pytest.fail("nudged"))
    render(_load("opamp_circ"), filepath=str(tmp_path), seed=1, nudge=False)


def test_nudged_layout_is_reproducible(tmp_path):
    doc = _load("opamp_circ")
    outs = []
    for name in ("a", "b"):
        out = tmp_path / name
        render(json.loads(json.dumps(doc)), filepath=str(out), top_name="sch", seed=3)
        outs.append((out / "sch.kicad_sch").read_text())
    assert outs[0] == outs[1]
