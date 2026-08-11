# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""
Lightweight intermediate representation (IR) for schematic generation.

The placement/routing/writer engine was originally written against live SKiDL
``Part``/``Pin``/``Net`` objects. To make ``schematizer`` independent of SKiDL
at runtime, this module provides small stand-ins that expose exactly the
attribute/method surface the engine reads and writes — and nothing else.

The engine also *sets* many transient attributes on these objects during place
and route (``force``, ``mv``, ``anchor_pins``, ``route_pt``, ``place_pt``, …).
Plain Python objects allow that, so those are not declared here; only the
attributes read before the engine sets them need initializing.

The :func:`schematizer.loader.load_netlist` function builds a graph of these
objects directly from the generic netlist JSON (no KiCad library access, no
re-parse).
"""

from .geometry import Tx

__all__ = [
    "SchPin",
    "SchNet",
    "NCNet",
    "SchPart",
    "SchPartUnit",
    "Circuit",
    "HIER_SEP",
]

# Separator between hierarchy levels in a part's hierarchical name. Matches
# SKiDL's convention so the generated UUIDs (uuid5 of hiername) stay stable.
HIER_SEP = "."


class _Lib:
    """Minimal stand-in for a SKiDL ``SchLib``.

    The writer only reads ``lib.filename`` to form the ``lib_id`` prefix
    (``"Device"`` in ``"Device:R"``).
    """

    def __init__(self, filename=""):
        self.filename = filename


class SchPin:
    """A single connection point on a part."""

    def __init__(self, num="", name="", x=0, y=0, orientation=0, part=None, net=None):
        self.num = num
        self.name = name
        # Native symbol coordinates (mm) and orientation (int degrees or, after
        # preprocessing, one of "R"/"U"/"L"/"D").
        self.x = x
        self.y = y
        self.orientation = orientation
        self.part = part
        self.net = net
        # Layout state, set/overwritten during preprocessing and routing.
        self.stub = False
        self.routed = False

    def is_connected(self):
        """True if the pin is attached to a real (non-no-connect) net."""
        return self.net is not None and not isinstance(self.net, NCNet)

    def is_attached(self, other):
        """True if this pin shares a net with another pin (used by ``similarity``)."""
        onet = getattr(other, "net", None)
        return self.net is not None and self.net is onet

    def __iadd__(self, net):
        """``pin += net`` connects the pin to the net."""
        net.connect(self)
        return self

    def __repr__(self):
        ref = getattr(self.part, "ref", "?")
        return "SchPin({}.{})".format(ref, self.num)


class SchNet:
    """An electrical connection between pins."""

    def __init__(self, name="", code=0, circuit=None):
        self.name = name
        self.code = code
        self._pins = []
        self._stub = False
        self._stub_explicit = False
        self._implicit = False
        self.netio = ""
        self.drive = 0
        self.valid = True
        self.do_erc = True
        self.circuit = circuit

    @property
    def pins(self):
        # Return a fresh list, mirroring SKiDL's Net.pins (which rebuilds the
        # list on each access). Engine code iterates ``net.pins`` while pins are
        # being appended (NetTerminals); the snapshot must not grow underfoot.
        return list(self._pins)

    @property
    def stub(self):
        return self._stub

    @stub.setter
    def stub(self, value):
        # Setting the net's stub cascades to every pin on the net, matching
        # SKiDL's Net.stub behavior.
        self._stub = value
        for pin in self._pins:
            pin.stub = value

    def is_implicit(self):
        """True if the net name was auto-generated (not user-assigned)."""
        return self._implicit

    def get_pins(self):
        return list(self._pins)

    def connect(self, pin):
        """Attach a pin to this net."""
        pin.net = self
        if pin not in self._pins:
            self._pins.append(pin)
        return self

    def __iadd__(self, pin):
        self.connect(pin)
        return self

    def __repr__(self):
        return "SchNet({})".format(self.name)


class NCNet(SchNet):
    """A no-connect net. Pins attached here are intentionally unconnected."""

    pass


class SchPart:
    """A placeable component instance."""

    def __init__(
        self,
        name="",
        ref="",
        ref_prefix="U",
        value="",
        footprint="",
        lib_id="",
        description="",
        datasheet="",
        tool=None,
        circuit=None,
    ):
        self.name = name
        self.ref_prefix = ref_prefix
        self.value = value
        self.footprint = footprint
        self.description = description
        self.datasheet = datasheet

        lib_name = lib_id.split(":", 1)[0] if lib_id else ""
        self.lib = _Lib(lib_name)

        self.pins = []
        self.unit = {}          # empty => single-unit part
        self.fields = {}
        self.symtx = ""
        self.orientation_locked = False
        self.tx = Tx()
        self.draw_cmds = {}     # {unit_num(int): [draw command lists]}
        self.hiertuple = ("",)  # hierarchy path; top level is "".
        self.parent = None      # set for PartUnits; None for whole parts
        self.num = 1            # unit number

        self.circuit = circuit
        self._ref = ref
        if circuit is not None:
            circuit.parts.append(self)
            if not self._ref:
                self._ref = circuit._next_ref(ref_prefix)

    # ref is a plain attribute, exposed as a property so NetTerminal (which
    # sets ref_prefix and lets the circuit assign a ref) behaves like a Part.
    @property
    def ref(self):
        return self._ref

    @ref.setter
    def ref(self, value):
        self._ref = value

    @property
    def tag_ref_name(self):
        return str(self._ref)

    @property
    def hiername(self):
        """Hierarchical name: hierarchy path joined with the reference."""
        return HIER_SEP.join(tuple(self.hiertuple) + (self.tag_ref_name,))

    @property
    def ordered_pins(self):
        """Pins in a stable order (used by ``similarity`` pin matching)."""
        return sorted(self.pins, key=lambda p: str(p.num))

    def grab_pins(self):
        """Reclaim pins from any PartUnits (no-op for single-unit parts)."""
        for unit in self.unit.values():
            unit.release_pins()

    def release_pins(self):
        """A whole part has no parent to release pins to."""
        pass

    def add_pins(self, *pins):
        """Attach pins to this part."""
        for pin in pins:
            pin.part = self
            self.pins.append(pin)
        return self

    def similarity(self, part, **options):
        """Heuristic similarity score with another part (higher = more alike).

        Reproduces SKiDL's ``Part.similarity`` so the placer groups like parts.
        """

        def score_pins():
            pin_score = 0
            if len(self.pins) == len(part.pins):
                for p_self, p_other in zip(self.ordered_pins, part.ordered_pins):
                    if p_self.is_attached(p_other):
                        pin_score += 1
            return pin_score

        score = 1
        if self.description == part.description:
            score += 5
        if self.name == part.name:
            score += 5
            if self.value == part.value:
                score += 2
            score += score_pins()
        elif self.ref_prefix == part.ref_prefix:
            score += 3
            if self.value == part.value:
                score += 2
            score += score_pins()

        return score / 3

    def __iter__(self):
        return iter(self.pins)

    def __len__(self):
        return len(self.pins)

    def __repr__(self):
        return "SchPart({})".format(self._ref or self.name)


class SchPartUnit(SchPart):
    """One unit of a multi-unit part (e.g. one amplifier in a quad op-amp).

    Units are what actually get placed: the node tree adds ``part.unit.values()``
    instead of the whole part, and the writer emits one symbol instance per unit,
    all sharing the parent's reference and distinguished by ``(unit N)``.

    Shared identity (name, library, value, symbol graphics, …) comes from the
    parent; the unit owns only its own subset of pins and its own placement.
    """

    def __init__(self, parent, num, label, pins):
        # Deliberately not calling SchPart.__init__: a unit must not register
        # itself as a separate part in the circuit.
        self.parent = parent
        self.num = num
        self.label = label
        self.pins = list(pins)

        # Identity shared with the parent part.
        self.name = parent.name
        self.lib = parent.lib
        self.value = parent.value
        self.footprint = parent.footprint
        self.description = parent.description
        self.datasheet = parent.datasheet
        self.ref_prefix = parent.ref_prefix
        self.draw_cmds = parent.draw_cmds
        self.fields = parent.fields
        self.circuit = parent.circuit

        # Per-unit placement state.
        self.symtx = parent.symtx
        self.orientation_locked = parent.orientation_locked
        self.tx = Tx()
        self.unit = {}  # A unit has no sub-units.

    @property
    def ref(self):
        """Compound reference ("U1.uA").

        The writer deliberately emits ``parent.ref`` for the KiCad reference
        (KiCad reads a compound ref as a distinct component), but the compound
        form still distinguishes units when generating UUIDs.
        """
        return HIER_SEP.join((self.parent.ref, self.label))

    @property
    def tag_ref_name(self):
        return self.ref

    @property
    def hiertuple(self):
        return self.parent.hiertuple

    def grab_pins(self):
        """Point this unit's pins at the unit (they place/route as the unit)."""
        for pin in self.pins:
            pin.part = self

    def release_pins(self):
        """Hand this unit's pins back to the parent part."""
        for pin in self.pins:
            pin.part = self.parent

    def __repr__(self):
        return "SchPartUnit({})".format(self.ref)


class Circuit:
    """Container for the IR parts and nets fed to the engine."""

    def __init__(self):
        self.parts = []
        self.nets = []
        self._ref_counts = {}

    def _next_ref(self, ref_prefix):
        """Assign a unique reference for a part created without one (e.g. NetTerminal)."""
        n = self._ref_counts.get(ref_prefix, 0) + 1
        self._ref_counts[ref_prefix] = n
        return "{}{}".format(ref_prefix, n)

    def get_nets(self):
        return self.nets

    def rmv_parts(self, *parts):
        """Remove parts (e.g. NetTerminals) from the circuit.

        Disconnects each removed part's pins from their nets too, so no stale
        pins linger on the nets. This matters for the place/route retry loop:
        NetTerminals added on one attempt are removed before the next, and a
        leftover terminal pin (with a root-level hiertuple) would otherwise
        break hierarchy lookup when the node tree is rebuilt.
        """
        for part in parts:
            for pin in part.pins:
                net = getattr(pin, "net", None)
                if net is not None:
                    try:
                        net._pins.remove(pin)
                    except (ValueError, AttributeError):
                        pass
                    pin.net = None
            try:
                self.parts.remove(part)
            except ValueError:
                pass
