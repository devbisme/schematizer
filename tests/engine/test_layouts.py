"""Sheets holding the same circuit are drawn alike, and a sheet can be laid out several times."""

import glob
import json
import os
import re
from collections import defaultdict

import pytest

from schematizer import render
from schematizer.engine import layouts
from schematizer.engine.layouts import Layout, match_nodes
from schematizer.geometry import BBox, Point, Segment, Tx

DATA = os.path.join(os.path.dirname(__file__), "..", "data")

_WIRE_RE = re.compile(
    r"\(wire\s*\(pts\s*\(xy\s+([-\d.]+)\s+([-\d.]+)\)\s*\(xy\s+([-\d.]+)\s+([-\d.]+)\)"
)


def _load(case):
    with open(os.path.join(DATA, f"{case}.json")) as f:
        return json.load(f)


def _wires(path):
    with open(path) as f:
        return sorted(tuple(map(float, w)) for w in _WIRE_RE.findall(f.read()))


def _render(case, tmp_path, **options):
    messages = []
    out = str(tmp_path / "out")
    render(_load(case), filepath=out, top_name="sch", progress=messages.append, **options)
    return out, messages


class _Obj:
    """A stand-in for a part, pin, net or node: attributes, and usable as a dict key."""

    def __init__(self, **attrs):
        self.__dict__.update(attrs)


def _part(name, x, y, nets):
    part = _Obj(name=name, tx=Tx(dx=x, dy=y), pins=[], bbox=BBox(Point(-20, -20), Point(20, 20)))
    for i, net in enumerate(nets):
        part.pins.append(
            _Obj(num=str(i + 1), orientation="L", stub=False, pt=Point(0, 50 * i), net=net, part=part)
        )
    return part


def _node(parts, wires=None):
    return _Obj(
        parts=parts,
        children={},
        flattened=False,
        wires=defaultdict(list, wires or {}),
        junctions=defaultdict(list),
        tx=Tx(),
        bbox=BBox(),
    )


def _divider(suffix, mid_pins=2):
    """Two resistors in series between a rail, a middle net and ground."""
    top, mid, gnd = (_Obj(name=n + suffix) for n in ("TOP", "MID", "GND"))
    r1, r2 = _part("R", 0, 0, [top, mid]), _part("R", 0, 200, [mid, gnd])
    wires = {mid: [Segment(Point(0, 50), Point(0, 200))]}
    return _node([r1, r2], wires), (r1, r2), (top, mid, gnd)


class TestLayout:
    def test_a_layout_goes_back_as_it_was_recorded(self):
        node, (r1, r2), (_, mid, _) = _divider("")
        layout = Layout(node)
        r1.tx = Tx(dx=999, dy=999)
        node.wires = defaultdict(list)
        layout.apply()
        assert (r1.tx.origin, r2.tx.origin) == (Point(0, 0), Point(0, 200))
        [seg] = node.wires[mid]
        assert (seg.p1, seg.p2) == (Point(0, 50), Point(0, 200))

    def test_a_recorded_layout_is_a_copy(self):
        node, _, (_, mid, _) = _divider("")
        layout = Layout(node)
        node.wires[mid][0].p2 = Point(0, 5000)
        layout.apply()
        assert node.wires[mid][0].p2 == Point(0, 200)


class TestMatchNodes:
    def test_the_same_circuit_matches_whatever_its_nets_are_called(self):
        (a, parts_a, nets_a), (b, parts_b, nets_b) = _divider("_A"), _divider("_B")
        match = match_nodes(a, b)
        assert [match.parts[p] for p in parts_a] == list(parts_b)
        assert [match.nets[n] for n in nets_a] == list(nets_b)

    def test_a_layout_is_copied_onto_the_matching_parts_and_nets(self):
        (a, _, _), (b, (r1, r2), (_, mid, _)) = _divider("_A"), _divider("_B")
        r2.tx, b.wires = Tx(dx=700, dy=700), defaultdict(list)
        Layout(a).apply(b, match_nodes(a, b))
        assert r2.tx.origin == Point(0, 200)
        assert [(s.p1, s.p2) for s in b.wires[mid]] == [(Point(0, 50), Point(0, 200))]

    def test_different_symbols_dont_match(self):
        (a, _, _), (b, (r1, _), _) = _divider("_A"), _divider("_B")
        r1.name = "C"
        assert match_nodes(a, b) is None

    def test_different_wiring_doesnt_match(self):
        (a, _, _), (b, (_, r2), (top, _, _)) = _divider("_A"), _divider("_B")
        # The second resistor goes back to the rail instead of to ground.
        r2.pins[1].net = top
        assert match_nodes(a, b) is None

    def test_a_different_number_of_parts_doesnt_match(self):
        (a, _, _), (b, _, _) = _divider("_A"), _divider("_B")
        b.parts.append(_part("R", 0, 400, [_Obj(name="X")]))
        assert match_nodes(a, b) is None


def test_sheets_with_the_same_circuit_are_drawn_alike(tmp_path):
    out, messages = _render("span_hier", tmp_path, seed=1)
    assert [m for m in messages if " like " in m] == ["drawing A:2 like A:1", "drawing A:3 like A:1"]
    first, second, third = (_wires(os.path.join(out, f"sch_A:{i}.kicad_sch")) for i in (1, 2, 3))
    assert first and first == second == third


def test_reuse_can_be_turned_off(tmp_path):
    _, messages = _render("span_hier", tmp_path, seed=1, reuse_layouts=False)
    assert not [m for m in messages if " like " in m]
    assert sum(m.startswith("routing A:") for m in messages) == 3


def test_only_matching_sheets_share_a_layout(tmp_path):
    """The board's two analog front ends are the same circuit; its other seven sheets differ."""
    _, messages = _render("dual_adc_usb", tmp_path, seed=1, check_netlist=True)
    assert [m for m in messages if " like " in m] == ["drawing afe_ch_b like afe_ch_a"]


def test_each_sheet_is_laid_out_sheet_tries_times(tmp_path):
    _, messages = _render("esp32_audio_board", tmp_path, seed=1, sheet_tries=3, check_netlist=True)
    assert any(m.startswith("layout 2/3,") for m in messages)
    assert any(m.startswith("layout 3/3,") for m in messages)
    assert not any(m.startswith("layout 4") for m in messages)


@pytest.mark.python_engine
def test_the_cheapest_layout_is_kept(tmp_path, monkeypatch):
    """Whatever the tries cost, a sheet ends up with the layout that cost least."""
    costs = iter([(5, 100), (3, 100), (4, 100)] * 50)
    seen = defaultdict(list)
    real = layouts.Layout

    def fake_cost(node):
        cost = next(costs)
        seen[node.name].append(cost)
        return cost

    kept = {}

    class Recorder(real):
        def apply(self, node=None, match=None):
            if node is None:
                kept[self.node.name] = self
            return super().apply(node, match)

    monkeypatch.setattr(layouts, "layout_cost", fake_cost)
    monkeypatch.setattr(layouts, "Layout", Recorder)
    made = []
    original_init = real.__init__

    def init(self, node):
        original_init(self, node)
        made.append(self)

    monkeypatch.setattr(real, "__init__", init)
    _render("hier", tmp_path, seed=1, sheet_tries=3)
    # A sheet that was costed three times kept the layout recorded after its second try.
    tried = [name for name, c in seen.items() if len(c) == 3]
    assert tried
    for name in tried:
        of_sheet = [l for l in made if l.node.name == name and l.node is kept[name].node]
        assert kept[name] is of_sheet[1]


def test_sheet_tries_keeps_a_seed_reproducible(tmp_path):
    outs = []
    for run in ("a", "b"):
        out, _ = _render("span_hier", tmp_path / run, seed=7, sheet_tries=3)
        outs.append({os.path.basename(f): open(f).read() for f in glob.glob(out + "/*.kicad_sch")})
    assert outs[0] == outs[1]


@pytest.mark.python_engine
def test_a_sheet_is_scored_as_its_file_will_be(tmp_path, monkeypatch):
    """Overlaps are counted from a routed sheet before it's written, so layouts can be compared."""
    from schematizer.engine import gen_schematic
    from schematizer.quality import score_overlaps

    scores = {}
    real = layouts.best_layout

    def best_layout(node, tries, tool=None, **options):
        kept = real(node, tries, tool=tool, **options)
        scores[node.name] = layouts.layout_score(node)
        return kept

    monkeypatch.setattr(layouts, "best_layout", best_layout)
    monkeypatch.setattr(gen_schematic, "best_layout", best_layout)
    out, _ = _render("esp32_audio_board", tmp_path, seed=1)

    written = {"part_overlaps": 0, "text_overlaps": 0}
    for path in glob.glob(out + "/*.kicad_sch"):
        with open(path) as f:
            for key, value in score_overlaps(f.read()).items():
                written[key] += value
    assert written["text_overlaps"] == 1  # So there's something to find.
    for key in written:
        assert sum(score[key] for score in scores.values()) == written[key]
    assert all(score["wirelength"] >= 0 and score["bends"] >= 0 for score in scores.values())


def test_overlaps_raise_a_sheets_cost(monkeypatch):
    score = {"bends": 2, "wirelength": 100.0, "part_overlaps": 0, "text_overlaps": 0}
    scores = iter([score, {**score, "text_overlaps": 1}, {**score, "part_overlaps": 1}])
    monkeypatch.setattr(layouts, "layout_score", lambda node: next(scores))
    clean, text, part = (layouts.layout_cost(None) for _ in range(3))
    assert text > clean and part > clean


def test_a_try_that_fails_to_route_gets_a_second_go(monkeypatch):
    from schematizer.engine.route import RoutingFailure

    node, _, (_, mid, _) = _divider("")
    calls = {"place": 0, "route": 0}

    def place(tool=None, **options):
        calls["place"] += 1

    def route(tool=None, **options):
        calls["route"] += 1
        if calls["route"] == 1:
            raise RoutingFailure

    node.place, node.route, node.name = place, route, "sheet"
    costs = iter([10, 5])
    monkeypatch.setattr(layouts, "layout_cost", lambda n: next(costs))
    assert layouts.best_layout(node, 2) == 1
    assert calls == {"place": 2, "route": 2}
