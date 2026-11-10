# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""
Drafting conventions people follow when drawing schematics.

* Supply rails are at the top and grounds at the bottom, so a pin on a supply
  net should have its wire leave upward and a pin on a ground net downward.
* Signals flow left to right, so wires should enter input pins from the left
  and leave output pins to the right.
* ICs are drawn the way their symbols were designed: they may be mirrored but
  not turned on their sides.

``orientation_cost()`` scores how badly an orientation of a part breaks these,
and the placer only uses the orientations with the lowest score.
"""

import re

from ..geometry import Tx, Vector

__all__ = [
    "is_power_net_name",
    "is_ground_net_name",
    "rail_dir",
    "linear",
    "pin_exit",
    "orientations",
    "orientation_cost",
    "best_orientations",
    "conventional_orientation",
]

# Names of nets drawn with power symbols instead of wires. This is the one
# definition of a power net name for the engine.
_POWER_NET_RE = re.compile(
    r"^(\+\d[\d.]*V[\d]*|GND|AGND|DGND|PGND|VCC|VDD|VSS|VEE|VBUS|VBAT|AVCC|AVDD|DVCC|DVDD)$",
    re.IGNORECASE,
)

# Power nets that go at the bottom of a drawing: grounds and negative supplies.
_GROUND_NET_RE = re.compile(r"GND|^(VSS|VEE|GROUND)$|^-", re.IGNORECASE)

# Direction a wire leaves a pin, by pin orientation, in unrotated part
# coordinates (Y points up). An "R" pin points right, into the part body, so
# it's on the part's left side and its wire leaves to the left.
PIN_EXIT = {"U": Vector(0, -1), "D": Vector(0, 1), "L": Vector(1, 0), "R": Vector(-1, 0)}

UP, DOWN, LEFT, RIGHT = (0, 1), (0, -1), (-1, 0), (1, 0)

# Pin functions with a signal direction, and the way their wires should leave.
_FLOW_EXIT = {"INPUT": LEFT, "OUTPUT": RIGHT}

# Parts with more pins than this are ICs that don't get turned on their sides.
MAX_ROTATED_PINS = 3

# Costs of breaking a convention, charged once for each pin that's sideways to
# the way it should face and twice if it's backwards. Turning an IC sideways is
# never worth it, and a rail pin facing wrong is worse than a signal doing so.
SIDEWAYS_IC_COST = 1000
RAIL_COST = 10
FLOW_COST = 1


def is_power_net_name(name):
    """Return True if a net with this name is a supply rail or a ground."""
    return name.startswith("+") or bool(_POWER_NET_RE.match(name))


def is_ground_net_name(name):
    """Return True if a net with this name is a ground or a negative supply."""
    return bool(_GROUND_NET_RE.search(name))


def rail_dir(net):
    """Return which way a net's power symbol points.

    Args:
        net (Net): Net attached to a pin, or None.

    Returns:
        tuple | None: UP for a supply rail, DOWN for a ground, None for a signal.
    """
    name = getattr(net, "name", "") or ""
    if is_ground_net_name(name):
        return DOWN
    if is_power_net_name(name):
        return UP
    return None


def linear(tx):
    """Return the rotation/flip part of a transform, without its translation."""
    return Tx(a=tx.a, b=tx.b, c=tx.c, d=tx.d)


def pin_exit(pin, tx):
    """Return the direction a pin's wire leaves it when its part has the given transform."""
    lin = linear(tx)
    exit_vec = PIN_EXIT.get(pin.orientation, Vector(0, 0))
    return (pin.pt + exit_vec) * lin - pin.pt * lin


def orientations(tx):
    """Return the eight orientations (four rotations, each mirrored or not) of a transform."""
    lin = linear(tx)
    txs = []
    for _ in range(2):
        for _ in range(4):
            txs.append(lin)
            lin = lin.rot_90cw()
        lin = lin.flip_x()
    return txs


def _wanted_exit(pin):
    """Return the direction a pin's wire should leave it and what it costs if it doesn't."""
    rail = rail_dir(pin.net)
    if rail:
        return rail, RAIL_COST
    flow = _FLOW_EXIT.get(getattr(pin, "func", ""))
    if flow and pin.net is not None:
        return flow, FLOW_COST
    return None, 0


def orientation_cost(part, tx=None):
    """Score how badly an orientation of a part breaks the drafting conventions.

    Args:
        part (Part): Part with pins that have .pt, .orientation and .net.
        tx (Tx, optional): Orientation to score. Defaults to the part's own.
            Only its rotation/flip matters.

    Returns:
        int: Zero if nothing is broken; larger is worse.
    """
    tx = part.tx if tx is None else tx
    cost = 0
    if len(part.pins) > MAX_ROTATED_PINS and (tx.b or tx.c):
        cost += SIDEWAYS_IC_COST
    for pin in part.pins:
        wanted, pin_cost = _wanted_exit(pin)
        if wanted:
            exit_vec = pin_exit(pin, tx)
            # Sideways costs once; backwards (dot product of -1) costs twice.
            along = round(exit_vec.x) * wanted[0] + round(exit_vec.y) * wanted[1]
            cost += pin_cost * (1 - along)
    # Satellites fixed beside the part (see preplace.py) turn with it.
    for rider in getattr(part, "riders", ()):
        cost += orientation_cost(rider, rider.rel_tx * tx)
    return cost


def best_orientations(part):
    """Return the orientations of a part that break the conventions the least.

    Returns:
        list: Transforms (rotation/flip only), starting with the part's current
            orientation if it's one of the best.
    """
    costed = [(orientation_cost(part, tx), tx) for tx in orientations(part.tx)]
    least = min(cost for cost, _ in costed)
    return [tx for cost, tx in costed if cost == least]


def conventional_orientation(part):
    """Return the orientation closest to the part's current one that best follows the conventions."""
    return best_orientations(part)[0]
