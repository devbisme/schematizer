# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""Regression tests for net-label direction, label deconfliction, and Ref/Value field layout.

The orientation expectations below were verified by exporting the generated
schematics with ``kicad-cli sch export svg`` and inspecting the renders.
"""

from types import SimpleNamespace

import pytest

from schematizer import quality
from schematizer.geometry import BBox, Point, Tx
from schematizer.engine import decisions
from schematizer.engine import sexp_schematic as sx
from schematizer.engine.net_terminal import NetTerminal

# Sheet transform as built by _calc_sheet_tx(): mils -> mm with a Y-flip.
SHEET_TX = Tx(a=sx.MILS_TO_MM, d=-sx.MILS_TO_MM)


def _pin(part, orientation, pt=Point(0, 0)):
    return SimpleNamespace(
        orientation=orientation,
        part=part,
        pt=pt,
        x=pt.x,
        y=pt.y,
        stub=True,
        net=SimpleNamespace(name="SIG"),
        is_connected=lambda: True,
    )


@pytest.mark.parametrize(
    "flipped,expected_dir",
    [
        (False, "R"),  # Label body to the left of the pin -> label extends left.
        (True, "L"),  # Output-net terminal flipped so its label extends right.
    ],
)
def test_net_terminal_label_points_away_from_routing(flipped, expected_dir):
    """A NetTerminal's label must extend over its own body, away from the wire
    entering its pin, not back across the parts it's attached to."""
    term = NetTerminal.__new__(NetTerminal)
    term.tx = Tx().flip_x() if flipped else Tx()
    pin = _pin(term, "L")
    assert sx.calc_label_dir(pin) == expected_dir


def test_real_part_label_dir_unchanged():
    part = SimpleNamespace(tx=Tx())
    for orientation in "UDLR":
        assert sx.calc_label_dir(_pin(part, orientation)) == orientation


class _Part(list):
    """Minimal part: an iterable of pins with placement geometry."""

    def __init__(self, place_bbox):
        super().__init__()
        self.place_bbox = place_bbox
        self.tx = Tx()


def _deconflict(labels, parts):
    backend = SimpleNamespace(label_bbox=lambda text: (10.0, 1.27))
    node = SimpleNamespace(parts=parts)
    return decisions.deconflict_labels(labels, [], node, backend, Tx())


def test_upward_label_above_body_is_not_moved():
    """KiCad label angle 90 points UP (sheet Y grows downward). A label sitting
    above a body and pointing up doesn't touch it, so it must stay put. The old
    (cos, sin) direction treated it as pointing down into the body, moved it
    below the body, and drew a wire straight through it."""
    body = _Part(BBox(Point(-2, 0), Point(2, 10)))
    moves, wires = _deconflict([(0, "VIN", 0.0, -1.27, 90)], [body])
    assert moves == [] and wires == []


def test_label_not_dragged_across_a_body():
    """A label pointing into a neighboring body must not be relocated past it,
    because the connecting wire would cut through that part (and its pins)."""
    body = _Part(BBox(Point(-12.7, -2.54), Point(-5.08, 2.54)))
    moves, wires = _deconflict([(0, "SIG", 0.0, 0.0, 180)], [body])
    assert moves == [] and wires == []


def _two_pin_vertical_part(tx):
    """Vertical 2-pin part (like Device:R): pins top & bottom, body 80 x 300 mils."""
    part = SimpleNamespace(tx=tx, body_bbox=BBox(Point(-40, -150), Point(40, 150)))
    part.pins = [_pin(part, "D", Point(0, 150)), _pin(part, "U", Point(0, -150))]
    return part


ORIENTATIONS = [
    Tx(),
    Tx().rot_90cw(),
    Tx().rot_90cw().rot_90cw(),
    Tx().rot_90cw().rot_90cw().rot_90cw(),
    Tx().flip_x(),
    Tx().rot_90cw().flip_x(),
    Tx().rot_90cw().rot_90cw().flip_x(),
    Tx().rot_90cw().rot_90cw().rot_90cw().flip_x(),
]


@pytest.mark.parametrize("tx", ORIENTATIONS)
def test_fields_sit_on_pin_free_side(tx):
    """Ref/Value go beside the body on a side with no pins: to the right of a
    vertical part, above/below a horizontal one -- never on the body."""
    part = _two_pin_vertical_part(tx.move(Point(5000, 5000)))
    body = part.body_bbox * part.tx * SHEET_TX
    (rx, ry, rj), (vx, vy, vj) = sx.calc_field_layout(part, SHEET_TX)
    if body.h > body.w:  # Vertical on the sheet.
        assert rj == vj == "left"
        assert rx > body.max.x and vx > body.max.x
        assert ry < vy
    else:
        assert rj is vj is None
        assert ry < body.min.y and vy > body.max.y


@pytest.mark.parametrize(
    "tx,expected",
    [
        # Render-verified: identity keeps sheet-space justification.
        (Tx(), (0, "left")),
        # Net horizontal flip (written as 180 + mirror x): KiCad mirrors the
        # justification, so sheet-left text must be stored as "right".
        (Tx().flip_x(), (0, "right")),
    ],
)
def test_kicad_field_orient_known_cases(tx, expected):
    assert sx._kicad_field_orient(tx, SHEET_TX, "left") == expected


@pytest.mark.parametrize("tx", ORIENTATIONS)
def test_kicad_field_orient_reads_left_to_right(tx):
    """Whatever the symbol orientation, the stored field angle/justify must map
    back to horizontal text extending to the right on the sheet."""
    angle, just = sx._kicad_field_orient(tx, SHEET_TX, "left")
    local = {
        (0, "left"): Point(1, 0),
        (0, "right"): Point(-1, 0),
        (90, "left"): Point(0, -1),
        (90, "right"): Point(0, 1),
    }[(angle, just)]
    m = tx * SHEET_TX
    v = local * Tx(d=-1) * Tx(a=m.a, b=m.b, c=m.c, d=m.d)
    assert v.x > 0 and abs(v.y) < 1e-9


@pytest.mark.parametrize(
    "angle,expected",
    [
        (0, (100.0, 53.81, None)),  # GND (intrinsic 270) hanging down: text below.
        (180, (100.0, 46.19, None)),  # GND pointing up: text above.
        (90, (103.81, 50.0, "left")),  # GND pointing right: text to the right.
        (270, (96.19, 50.0, "right")),  # GND pointing left: text to the left.
    ],
)
def test_power_symbol_value_beyond_body(angle, expected, monkeypatch):
    """A power symbol's Value text goes past the end of the symbol, away from
    the pin -- not fixed above the connection point, where a downward GND would
    print its name over the part it's attached to."""
    monkeypatch.setattr(sx, "pwr_symbol_sexp_dict", {}, raising=False)
    sym = sx.power_symbol_instance_sexp("GND", 100.0, 50.0, angle, "#PWR01", "/", "t")
    val = next(el for el in sym if isinstance(el, list) and el[:2] == ["property", "Value"])
    at = next(el for el in val if isinstance(el, list) and el and el[0] == "at")
    effects = next(el for el in val if isinstance(el, list) and el and el[0] == "effects")
    just = next((el[1] for el in effects if isinstance(el, list) and el[0] == "justify"), None)
    assert (at[1], at[2], just) == expected


def _power_values(xs, monkeypatch, name="GND", angle=0):
    """Power symbols at the given X positions, after crowded names are turned."""
    monkeypatch.setattr(sx, "pwr_symbol_sexp_dict", {}, raising=False)
    symbols = [
        sx.power_symbol_instance_sexp(name, x, 50.0, angle, f"#PWR{i}", "/", f"t{i}")
        for i, x in enumerate(xs)
    ]
    sx.turn_crowded_power_values(symbols)
    return [
        (sym, next(el for el in sym if isinstance(el, list) and el[:2] == ["property", "Value"]))
        for sym in symbols
    ]


def _value_box(symbol, value):
    return quality._field_box(symbol, value, quality._instance_map(symbol))


def test_power_names_on_neighboring_pins_are_turned_upright(monkeypatch):
    """Rail names are wider than the 100 mils between IC pins, so side by side they'd overlap."""
    values = _power_values([100.0, 102.54, 105.08], monkeypatch)
    for symbol, value in values:
        at = quality._field(value, "at")
        assert at[3] == 90 and at[1] == quality._field(symbol, "at")[1]
        assert quality._field(quality._field(value, "effects"), "justify") is None
        # The name starts beyond the symbol's body, which is 2.54 mm long.
        _, top, _, bottom = _value_box(symbol, value)
        assert min(abs(top - 50.0), abs(bottom - 50.0)) >= 2.54
    boxes = [_value_box(symbol, value) for symbol, value in values]
    assert not any(quality._overlap(a, b) for i, a in enumerate(boxes) for b in boxes[i + 1 :])


def test_power_names_with_room_stay_level(monkeypatch):
    for _, value in _power_values([100.0, 120.0], monkeypatch):
        assert quality._field(value, "at")[3] == 0


def test_power_names_beside_their_symbols_stay_level(monkeypatch):
    """Names of symbols lying on their sides are a line apart and are left alone."""
    monkeypatch.setattr(sx, "pwr_symbol_sexp_dict", {}, raising=False)
    symbols = [
        sx.power_symbol_instance_sexp("GND", 100.0, y, 90, f"#PWR{i}", "/", f"t{i}")
        for i, y in enumerate((50.0, 51.0))
    ]
    sx.turn_crowded_power_values(symbols)
    for sym in symbols:
        value = next(el for el in sym if isinstance(el, list) and el[:2] == ["property", "Value"])
        assert quality._field(value, "at")[3] == 0


def test_upright_power_name_is_drawn_upright_in_svg():
    from schematizer.engine import svg_schematic

    sx.init_power_symbol_data()
    if "GND" not in sx.pwr_symbol_sexp_dict:
        pytest.skip("KiCad power library not found")
    svg, bbox = svg_schematic.power_symbol_to_svg("GND", 100.0, 50.0, 0, upright_value_at=(100.0, 55.0))
    assert svg.count("rotate(-90 100.000 55.000)") == 2  # The text and its white backing.
    assert bbox.max.y > 55.0
    level, _ = svg_schematic.power_symbol_to_svg("GND", 100.0, 50.0, 0)
    assert "rotate(-90" not in level and ">GND<" in level
