# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""Check that a written KiCad schematic connects what its netlist says.

KiCad exports the netlist of the schematic that was written, and each net's
pins are compared with the pins of the nets in the generic netlist document.
This catches what the router's own checks can't see: two nets joined by
touching wires, or a label, sheet pin or power symbol that doesn't connect.
"""

import os
import re
import shutil
import subprocess
import tempfile

__all__ = ["NetlistMismatch", "check_netlist", "source_nets", "exported_nets", "compare_nets"]

# How many differing nets an error message lists.
MAX_REPORTED = 8


class NetlistMismatch(Exception):
    """The schematic that was written doesn't have the netlist's connections.

    Attributes:
        problems (list): One sentence for each net that differs.
    """

    def __init__(self, problems):
        self.problems = list(problems)
        shown = self.problems[:MAX_REPORTED]
        more = len(self.problems) - len(shown)
        text = "; ".join(shown) + (f"; and {more} more" if more else "")
        super().__init__(f"The schematic doesn't match its netlist: {text}.")


def _counts(pin):
    """KiCad leaves "#" references (power symbols, flags) out of its netlists."""
    return not pin[0].startswith("#")


def source_nets(doc):
    """Return {net name: {(ref, pin), ...}} for the nets of a netlist document.

    The no-connect net and nets with fewer than two pins are left out, since
    they connect nothing.
    """
    nets = {}
    for net in doc.get("nets", ()):
        if net.get("nc"):
            continue
        pins = {(node["ref"], str(node["pin"])) for node in net.get("nodes", ())}
        pins = frozenset(pin for pin in pins if _counts(pin))
        if len(pins) > 1:
            nets[net.get("name") or f"net {net.get('code')}"] = pins
    return nets


def exported_nets(schematic_file):
    """Return the pin sets of the nets KiCad finds in a schematic file.

    Raises:
        RuntimeError: kicad-cli isn't installed, or couldn't export the netlist.
    """
    kicad_cli = shutil.which("kicad-cli")
    if not kicad_cli:
        raise RuntimeError("Checking the netlist needs kicad-cli, which isn't installed.")
    work_dir = tempfile.mkdtemp()
    try:
        netlist_file = os.path.join(work_dir, "exported.net")
        cmd = [kicad_cli, "sch", "export", "netlist", "--format", "kicadsexpr",
               "-o", netlist_file, schematic_file]
        try:
            result = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
        except (subprocess.TimeoutExpired, OSError) as e:
            raise RuntimeError(f"kicad-cli couldn't export the netlist: {e}") from e
        if not os.path.exists(netlist_file):
            detail = (result.stderr or result.stdout).strip()
            raise RuntimeError(f"kicad-cli couldn't export the netlist: {detail}")
        with open(netlist_file) as f:
            text = f.read()
    finally:
        shutil.rmtree(work_dir, ignore_errors=True)

    start = text.find("(nets")
    nets = []
    for block in re.split(r"\(net \(code", text[start:] if start >= 0 else "")[1:]:
        pins = re.findall(r'\(node \(ref "([^"]+)"\) \(pin "([^"]+)"\)', block)
        pins = frozenset(pin for pin in pins if _counts(pin))
        if len(pins) > 1:
            nets.append(pins)
    return nets


def _pins_text(pins, limit=4):
    names = sorted(f"{ref}.{num}" for ref, num in pins)
    more = len(names) - limit
    return ", ".join(names[:limit]) + (f" and {more} more" if more > 0 else "")


def compare_nets(source, drawn):
    """Return a sentence for each way the drawn nets differ from the source nets.

    Args:
        source (dict): {net name: pin set} from :func:`source_nets`.
        drawn (list): Pin sets from :func:`exported_nets`.
    """
    net_of = {pin: name for name, pins in source.items() for pin in pins}
    group_of = {pin: i for i, pins in enumerate(drawn) for pin in pins}

    problems = []
    reported_shorts = set()
    for name, pins in sorted(source.items()):
        if pins in drawn:
            continue
        groups = {group_of.get(pin) for pin in pins}

        # Joined to pins of other nets.
        for i in sorted(g for g in groups if g is not None):
            extra = drawn[i] - pins
            others = sorted({net_of[pin] for pin in extra if pin in net_of})
            stray = {pin for pin in extra if pin not in net_of}
            if stray:
                others.append(_pins_text(stray))
            if others and i not in reported_shorts:
                reported_shorts.add(i)
                problems.append(f"{name} is shorted to {', '.join(others)}")

        # Split up, or missing pins.
        loose = {pin for pin in pins if pin not in group_of}
        pieces = len(groups - {None}) + len(loose)
        if not groups - {None}:
            problems.append(f"none of the pins of {name} are connected ({_pins_text(pins)})")
        elif pieces > 1:
            biggest = max((drawn[g] & pins for g in groups if g is not None), key=len, default=frozenset())
            apart = pins - biggest
            problems.append(f"{name} is in {pieces} pieces ({_pins_text(apart)} not connected to the rest)")

    # Pins the netlist leaves alone that the drawing joined to each other.
    for i, pins in enumerate(drawn):
        if i not in reported_shorts and not any(pin in net_of for pin in pins):
            problems.append(f"{_pins_text(pins)} are connected but shouldn't be")
    return problems


def check_netlist(doc, schematic_file):
    """Raise NetlistMismatch if the schematic's connections differ from the netlist document's.

    Args:
        doc (dict): The generic netlist document the schematic was drawn from.
        schematic_file (str): The top-level .kicad_sch file.

    Raises:
        NetlistMismatch: The connections differ.
        RuntimeError: kicad-cli isn't installed, or couldn't export the netlist.
    """
    problems = compare_nets(source_nets(doc), exported_nets(schematic_file))
    if problems:
        raise NetlistMismatch(problems)
