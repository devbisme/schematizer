# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""
Generate a KiCad 9 schematic from a Circuit object.

Thin wrapper around the shared sexp_schematic module.
Uses SKiDL's placement and routing infrastructure.
"""

import contextlib
import os
import re
import shutil
import subprocess
from collections import defaultdict

from ..geometry import BBox, Point, Tx, Vector
from ..ir import NCNet
from .net_terminal import NetTerminal
from . import progress
from .rng import seed as rng_seed
from .snap import snap_two_pin_parts as _snap_two_pin_parts
from .._utils import get_script_name
from .._utils import export_to_all, rmv_attr

from .sexp_schematic import write_top_schematic
from .svg_schematic import write_top_svg
from .bboxes import calc_hier_label_bbox, calc_symbol_bbox
from .conventions import conventional_orientation, is_power_net

__all__ = []


# Which KiCad version this engine targets. ``render`` sets it before calling
# ``gen_schematic`` so the (tiny) per-version output toggles apply.
TARGET_TOOL = "kicad9"


def _setup_kicad_env():
    """Set KiCad footprint directory if not already set.

    Auto-detects the standard KiCad footprint directory so that
    generated schematics can reference footprints for PCB layout.
    """
    kicad_version = TARGET_TOOL[len("kicad") :]
    if not os.environ.get(f"KICAD{kicad_version}_FOOTPRINT_DIR"):
        for path in [
            "/usr/share/kicad/footprints",
            "/usr/local/share/kicad/footprints",
            os.path.expanduser(f"~/.local/share/kicad/{kicad_version}.0/footprints"),
        ]:
            if os.path.isdir(path):
                os.environ[f"KICAD{kicad_version}_FOOTPRINT_DIR"] = path
                break


# Suppress legacy fp-lib-table warnings from older KiCad tool modules.
import warnings

warnings.filterwarnings("ignore", message=".*fp-lib-table.*")


# Pattern matching common power net names.
FIXABLE_ERROR_TYPES = frozenset(
    {"pin_not_connected", "pin_not_driven", "wire_not_connected"}
)


def stub_power_nets(circuit, keep_drawn=False):
    """Stub the power nets so they're drawn with power symbols instead of wires.

    A supply rail or ground usually goes to most of the parts on a sheet.
    People don't wire those together; they put a power symbol on each pin
    (or a label, for a rail whose name has no power symbol).
    Nets whose stubbing was set explicitly by the user are left alone.

    Args:
        circuit: The Circuit object containing the nets.
        keep_drawn (bool): Also leave alone the nets that already have a power
            symbol part of their own, since that's how the user drew them.

    Returns:
        list: The nets that were stubbed.
    """
    stubbed = []
    for net in circuit.nets:
        if getattr(net, "_stub_explicit", False):
            continue
        if not net.valid or len(net.pins) == 0:
            continue
        if not is_power_net(net):
            continue
        if keep_drawn and any(
            getattr(pin.part, "is_power_symbol", False) for pin in net.pins
        ):
            continue
        net._stub = True
        net._stub_explicit = False
        for pin in net.get_pins():
            pin.stub = True
        stubbed.append(net)
    return stubbed


def auto_stub_nets(circuit, **options):
    """Auto-stub power nets and high-fanout nets before generation.

    Only modifies nets that haven't been explicitly set by the user.
    Called when auto_stub=True is passed to gen_schematic().

    Power nets are always stubbed immediately (see stub_power_nets()).
    Fanout-based stubbing is DEFERRED: nets are marked ``_deferred_stub`` so
    the placer still uses them for connectivity-based grouping, then they get
    stubbed after placement via ``_apply_deferred_stubs()``. Stubbing them up
    front instead scatters connected parts and makes dense boards (e.g. the
    327-part Concentric board) fail to route, which suppresses snap placement.

    Args:
        circuit: The Circuit object containing nets to analyze.
        options: Dict of options. Recognizes 'auto_stub_fanout' (default 5;
            gen_schematic() sets it from the auto_stub pin count).
    """
    fanout_threshold = options.get("auto_stub_fanout", 5)
    deferred_fanout = []
    stubbed_power = [f"{net.name}({len(net.pins)})" for net in stub_power_nets(circuit)]

    for net in circuit.nets:
        if getattr(net, "_stub_explicit", False) or net._stub:
            continue
        if not net.valid or len(net.pins) == 0:
            continue

        if len(net.pins) >= fanout_threshold:
            net._deferred_stub = True
            deferred_fanout.append(f"{net.name}({len(net.pins)})")

    from .._utils import active_logger

    active_logger.info(
        f"  [auto_stub] power: {', '.join(stubbed_power[:10])}{'...' if len(stubbed_power) > 10 else ''}"
    )
    active_logger.info(
        f"  [auto_stub] deferred fanout>={fanout_threshold}: {', '.join(deferred_fanout[:10])}{'...' if len(deferred_fanout) > 10 else ''}"
    )


def stub_lone_pins(circuit):
    """Stub a pin that's the only one of its net on a sheet, so it's drawn with a label.

    The label is what connects such a pin to the rest of its net on other
    sheets. Putting it on the pin saves placing a label somewhere else on the
    sheet and routing a wire to it.

    Args:
        circuit: The Circuit object containing the nets.
    """
    for net in circuit.nets:
        if net._stub or not net.valid or isinstance(net, NCNet):
            continue
        sheets = defaultdict(list)
        for pin in net.pins:
            if not isinstance(pin.part, NetTerminal):
                sheets[pin.part.hiertuple].append(pin)
        if len(sheets) < 2 and len(net.pins) != 1:
            # All of the net is on one sheet, where it's wired.
            continue
        for pins in sheets.values():
            # (A power symbol labels its net by itself.)
            if len(pins) == 1 and not getattr(pins[0].part, "is_power_symbol", False):
                pins[0].stub = True


def _new_node(circuit, tool_module, filepath, top_name, title, flatness, options):
    """Get the circuit ready for placement and build its tree of nodes."""
    from .sch_node import SchNode

    stub_lone_pins(circuit)
    preprocess_circuit(circuit, **options)
    return SchNode(
        circuit,
        tool_module,
        filepath,
        top_name,
        title,
        flatness,
        label_local_nets=options.get("label_local_nets", False),
    )


def _apply_deferred_stubs(node, circuit, **options):
    """Apply deferred stubs per-subcircuit after placement.

    Nets marked _deferred_stub are examined within each child node.
    Simple internal connections (few pins, close together) remain as wires;
    complex or distant ones get stubbed.

    NetTerminal parts (schematic-only label placeholders) are excluded from
    the pin count so they don't inflate it beyond the real circuit connectivity.
    """
    from .._utils import active_logger

    # Reset pin.stub for all deferred-stub nets (supports retries).
    for net in circuit.nets:
        if getattr(net, "_deferred_stub", False):
            net._stub = False
            for p in net.get_pins():
                p.stub = False

    for child in node.children.values():
        child_name = getattr(child, "name", "?")
        child_parts = set(child.parts)
        stubbed = 0
        seen = set()

        for part in child.parts:
            for pin in part:
                if not pin.is_connected():
                    continue
                net = pin.net
                if id(net) in seen:
                    continue
                seen.add(id(net))

                if not getattr(net, "_deferred_stub", False):
                    continue

                internal_pins = [p for p in net.pins if p.part in child_parts]

                # Deferred nets were kept un-stubbed during placement
                # for connectivity-aware grouping.  Now stub them all —
                # labels at pins are cleaner than routed wires for
                # cross-sheet connections.
                for p in internal_pins:
                    p.stub = True
                stubbed += 1

        if stubbed:
            active_logger.info(
                f"  [deferred_stub] {child_name}: {stubbed} deferred nets stubbed"
            )

    # Set net._stub for nets where ALL pins ended up stubbed.
    for net in circuit.nets:
        if getattr(net, "_deferred_stub", False):
            has_unstubbed = any(not p.stub for p in net.pins)
            if not has_unstubbed:
                net._stub = True
                net._stub_explicit = False

    # Resetting the deferred nets above also unstubbed their lone pins.
    stub_lone_pins(circuit)


def _run_erc(schematic_path):
    """Run kicad-cli ERC on a schematic file and return the report path.

    Args:
        schematic_path: Path to the .kicad_sch file.

    Returns:
        str: Path to the ERC report file, or None if kicad-cli is unavailable.
    """
    report_path = schematic_path.replace(".kicad_sch", "-erc.rpt")
    try:
        subprocess.run(
            [
                "kicad-cli",
                "sch",
                "erc",
                "--output",
                report_path,
                "--severity-all",
                schematic_path,
            ],
            capture_output=True,
            timeout=60,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None
    return report_path if os.path.exists(report_path) else None


def _parse_erc_report(report_path):
    """Parse kicad-cli ERC report and return list of (error_type, symbol_ref, pin_num).

    The ERC report format has lines like:
        [pin_not_connected]: Pin not connected ...
        @(x,y): Symbol U1 Pin 3 ...

    Args:
        report_path: Path to the ERC .rpt file.

    Returns:
        list: List of (error_type, symbol_ref, pin_num) tuples.
    """
    if not report_path or not os.path.exists(report_path):
        return []

    errors = []
    current_error_type = None

    # Patterns for ERC report parsing.
    error_type_re = re.compile(r"^\[(\w+)\]")
    symbol_pin_re = re.compile(r"Symbol\s+(\S+)\s+Pin\s+(\S+)")

    with open(report_path, "r") as f:
        for line in f:
            line = line.strip()

            # Match error type header.
            m = error_type_re.match(line)
            if m:
                current_error_type = m.group(1)
                continue

            # Match symbol/pin reference in the detail line.
            if current_error_type:
                m = symbol_pin_re.search(line)
                if m:
                    errors.append((current_error_type, m.group(1), m.group(2)))
                    current_error_type = None

    return errors


def _stub_nets_for_erc_errors(circuit, errors):
    """Convert nets involved in ERC errors to stubs for regeneration.

    Args:
        circuit: The Circuit object.
        errors: List of (error_type, symbol_ref, pin_num) from _parse_erc_report.

    Returns:
        bool: True if any nets were newly stubbed.
    """
    stubbed_any = False
    for error_type, symbol_ref, pin_num in errors:
        if error_type not in FIXABLE_ERROR_TYPES:
            continue
        for part in circuit.parts:
            if part.ref == symbol_ref:
                for pin in part.pins:
                    if str(pin.num) == str(pin_num):
                        net = pin.net
                        if net and not getattr(net, "_stub_explicit", False):
                            pins = net.get_pins()
                            # (If every pin already has a label, regenerating
                            # the drawing wouldn't change it.)
                            if not all(p.stub for p in pins):
                                stubbed_any = True
                            net._stub = True
                            net._stub_explicit = False
                            for p in pins:
                                p.stub = True
                break
    return stubbed_any


def _classify_and_stub_complex_nets(circuit, node, **options):
    """Classify nets after placement: stub complex ones, keep simple ones as wires.

    Called after placement succeeds, before routing. Nets with too many pins
    or pins too far apart get converted to labels for reliable connectivity.
    Simple 2-3 pin short-distance nets remain as wires.

    Args:
        circuit: The Circuit object.
        node: The placed SchNode.
        options: Dict of options including:
            auto_stub_max_wire_pins (int): Max pins for wire routing. Default 3.
            auto_stub_max_wire_dist (int): Max manhattan distance (mils) for wires. Default 2000.
    """
    from ..geometry import Point

    max_wire_pins = options.get("auto_stub_max_wire_pins", 3)
    max_wire_dist = options.get("auto_stub_max_wire_dist", 2000)

    def _classify_node(target_node):
        target_parts = set(target_node.parts)
        count = 0

        for net in target_node.get_internal_nets():
            if getattr(net, "_stub_explicit", False):
                continue
            if getattr(net, "_stub", False):
                continue

            pins = [
                p
                for p in net.pins
                if p.part in target_parts and not isinstance(p.part, NetTerminal)
            ]

            if len(pins) > max_wire_pins:
                net._stub = True
                net._stub_explicit = False
                for p in net.get_pins():
                    p.stub = True
                count += 1
                continue

            if len(pins) >= 2:
                pts = []
                for p in pins:
                    pin_pt = getattr(p, "place_pt", getattr(p, "pt", Point(p.x, p.y)))
                    part_tx = getattr(p.part, "tx", None)
                    if part_tx:
                        pts.append(pin_pt * part_tx)
                    else:
                        pts.append(pin_pt)

                max_dist = 0
                for i, a in enumerate(pts):
                    for b in pts[i + 1 :]:
                        dist = abs(a.x - b.x) + abs(a.y - b.y)
                        if dist > max_dist:
                            max_dist = dist

                if max_dist > max_wire_dist:
                    net._stub = True
                    net._stub_explicit = False
                    for p in net.get_pins():
                        p.stub = True
                    count += 1

        return count

    total_stubbed = _classify_node(node)
    for child in node.children.values():
        total_stubbed += _classify_node(child)

    # Second pass on children: stub nets that can't benefit from wiring.
    for child in node.children.values():
        child_parts = set(child.parts)
        child_internal = child.get_internal_nets()
        non_stubbed = [n for n in child_internal if not getattr(n, "_stub", False)]

        # Stub single-real-pin nets (cross-sheet labels, not wireable).
        for net in non_stubbed:
            real_pins = [
                p
                for p in net.pins
                if p.part in child_parts and not isinstance(p.part, NetTerminal)
            ]
            if len(real_pins) < 2:
                net._stub = True
                net._stub_explicit = False
                for p in net.get_pins():
                    p.stub = True
                total_stubbed += 1

        # Re-count after single-pin removal.
        non_stubbed = [n for n in child_internal if not getattr(n, "_stub", False)]

        # Stub all remaining nets in small subcircuits (<=6 routeable nets).
        # Only large chain structures (switch->R->IC grids) benefit from wiring.
        # A flattened subcircuit shares its parent's page, so wires still fit.
        # (Pre-placed parts sit beside the pins they're wired to, so their
        # short wires are kept whatever the size of the subcircuit.)
        if (
            not options.get("preplace", True)
            and not child.flattened
            and len(non_stubbed) <= 6
        ):
            for net in non_stubbed:
                net._stub = True
                net._stub_explicit = False
                for p in net.get_pins():
                    p.stub = True
                total_stubbed += 1

    if total_stubbed:
        from .._utils import active_logger

        active_logger.info(
            f"  [selective_routing] Stubbed {total_stubbed} complex/distant nets after placement"
        )


class LabelsOnlyWarning(UserWarning):
    """Warning raised when schematic falls back to labels-only output."""

    pass


def _handle_fallback(
    circuit,
    tool_module,
    filepath,
    top_name,
    title,
    flatness,
    options,
    logger,
    reason="",
):
    """Handle routing failure fallback according to the auto_stub_fallback policy.

    Args:
        circuit: The Circuit object.
        tool_module: The KiCad tool module.
        filepath, top_name, title, flatness: Schematic generation parameters.
        options: Dict of options including auto_stub_fallback policy.
        logger: The active logger.
        reason: Human-readable explanation of why we're falling back.
    """
    import warnings

    from .sch_node import SchNode

    fallback = options.get("auto_stub_fallback", "labels")

    if fallback == "raise":
        finalize_parts_and_nets(circuit, **options)
        from .route import RoutingFailure

        raise RoutingFailure(
            f"{reason}. Set auto_stub_fallback='labels' to produce "
            "labels-only output instead of crashing."
        )

    # Produce labels-only output.
    stubbed_nets = []
    for net in circuit.nets:
        if not getattr(net, "_stub_explicit", False) and not net._stub:
            stubbed_nets.append(net.name)
    _stub_all_non_explicit(circuit)

    node = _new_node(circuit, tool_module, filepath, top_name, title, flatness, options)
    node.place(expansion_factor=1.0, **options)
    node.route(**options)
    _snap_two_pin_parts(node)
    output_file = _write_output(circuit, node, filepath, top_name, title, options)
    finalize_parts_and_nets(circuit, **options)

    msg = (
        f"{reason}. Produced labels-only schematic at {output_file}. "
        f"Nets converted to labels: {', '.join(stubbed_nets[:10])}"
        f"{'...' if len(stubbed_nets) > 10 else ''}. "
        "This may mask routing issues that could be fixed by improving "
        "the circuit layout. Set auto_stub_fallback='raise' to get the "
        "original error instead."
    )
    logger.warning(msg)

    if fallback == "warn":
        warnings.warn(msg, LabelsOnlyWarning, stacklevel=4)


def _stub_all_non_explicit(circuit):
    """Stub all nets that weren't explicitly set by the user (labels-only fallback).

    Args:
        circuit: The Circuit object.
    """
    for net in circuit.nets:
        if not getattr(net, "_stub_explicit", False):
            net._stub = True
            for pin in net.get_pins():
                pin.stub = True


def preprocess_circuit(circuit, **options):
    """Add stuff to parts & nets for doing placement and routing of schematics."""

    def units(part):
        if len(part.unit) == 0:
            return [part]
        else:
            return part.unit.values()

    def initialize(part):
        """Initialize part or its part units."""

        pin_limit = options.get("orientation_pin_limit", 44)

        # KiCad 6+ stores pin orientation as integer degrees; normalize to string.
        deg_to_orient = {0: "R", 90: "U", 180: "L", 270: "D"}

        for part_unit in units(part):
            part_unit.tx = Tx.from_symtx(getattr(part_unit, "symtx", ""))

            num_pins = len(part_unit.pins)
            part_unit.orientation_locked = getattr(part_unit, "symtx", False) or not (
                1 < num_pins <= pin_limit
            )

            part_unit.grab_pins()

            for pin in part_unit:
                # Normalize pin orientation from integer degrees to string direction.
                if isinstance(pin.orientation, int):
                    pin.orientation = deg_to_orient.get(pin.orientation % 360, "R")
                # Pin coords from KiCad 9 libs are in mm; convert to mils
                # so the placement/routing engine works in consistent units.
                MM_TO_MILS = 1 / 0.0254
                pin.pt = Point(pin.x * MM_TO_MILS, pin.y * MM_TO_MILS)
                pin.routed = False

    def orient_conventionally(part):
        """Orient a part so rail pins face up, ground pins down, and signals flow left to right."""

        if getattr(part, "symtx", ""):
            # The user set this part's orientation, so leave it alone.
            return

        if getattr(part, "is_power_symbol", False):
            # A power symbol is drawn the way its library draws it. (Its pin's
            # wire leaves toward the circuit, the opposite of a part's pin on
            # the same rail.)
            return

        dont_rotate_pin_cnt = options.get("dont_rotate_pin_count", 10000)

        for part_unit in units(part):
            if len(part_unit) > dont_rotate_pin_cnt:
                return
            part_unit.tx = conventional_orientation(part_unit)

    def calc_part_bbox(part):
        """Calculate the labeled bounding boxes and store it in the part."""

        bare_bboxes = calc_symbol_bbox(part)[1:]

        for part_unit, bare_bbox in zip(units(part), bare_bboxes):
            resize_wh = Vector(0, 0)
            if bare_bbox.w < 100:
                resize_wh.x = (100 - bare_bbox.w) / 2
            if bare_bbox.h < 100:
                resize_wh.y = (100 - bare_bbox.h) / 2
            bare_bbox = bare_bbox.resize(resize_wh)

            # Bare symbol bbox (no labels) is used by the writers to place Ref/Value fields.
            part_unit.body_bbox = bare_bbox
            part_unit.lbl_bbox = BBox()
            part_unit.lbl_bbox.add(bare_bbox)
            for pin in part_unit:
                if pin.stub:
                    hlbl_bbox = calc_hier_label_bbox(pin.net.name, pin.orientation)
                    hlbl_bbox *= Tx().move(pin.pt)
                    part_unit.lbl_bbox.add(hlbl_bbox)

            part_unit.bbox = part_unit.lbl_bbox

    for part in circuit.parts:
        initialize(part)
        orient_conventionally(part)
        calc_part_bbox(part)


def finalize_parts_and_nets(circuit, **options):
    """Restore parts and nets after place & route is done."""

    net_terminals = (p for p in circuit.parts if isinstance(p, NetTerminal))
    circuit.rmv_parts(*net_terminals)

    for part in circuit.parts:
        part.grab_pins()

    rmv_attr(circuit.parts, ("force", "bbox", "lbl_bbox", "body_bbox", "tx"))


def _place_and_classify(node, circuit, expansion_factor, classify=True, **options):
    """Place a node and, when auto_stub is on, run the stub classifiers.

    Extracted so the main pass and the ERC-regeneration pass share one code
    path. ``classify=False`` places without running ``_apply_deferred_stubs`` /
    ``_classify_and_stub_complex_nets`` — used by the label-clearance re-place
    pass, which must place against an already-frozen stub set (re-running the
    classifiers would reset deferred pins and mutate the stub set after the
    label-aware bboxes were computed).
    """
    node.place(expansion_factor=expansion_factor, **options)
    if classify and options.get("auto_stub", False):
        _apply_deferred_stubs(node, circuit, **options)
        _classify_and_stub_complex_nets(circuit, node, **options)


def _snap_unwired_parts(node, options):
    """Snap two-pin parts onto the pins they connect to, if auto-stubbing left them loose.

    Snapping moves parts after routing and turns their wires into labels. It's
    for force-directed placement, which drops such parts anywhere. Pre-placed
    parts are already beside their pins with wires routed to them, and
    snapping would undo that and can land them on top of each other.
    """
    if options.get("auto_stub", False) and not options.get("preplace", True):
        _snap_two_pin_parts(node)


@export_to_all
def _write_output(circuit, node, filepath, top_name, title, options):
    """Serialize the placed+routed node tree in the requested output format.

    Placement and routing are format-agnostic -- only this final step differs,
    so the SVG pages have exactly the layout of the KiCad sheets.
    """
    progress.report("writing files")
    place_fields = options.get("place_fields", True)
    if options.get("output_format", "kicad") == "svg":
        return write_top_svg(
            circuit, node, filepath, top_name, title,
            fit=options.get("svg_fit", True),
            place_fields=place_fields,
        )
    # The ERC correction loop checks every sheet it writes, so don't check twice.
    erc_loop = options.get("auto_stub", False) and options.get("erc_max_iterations", 3) > 0
    return write_top_schematic(
        circuit,
        node,
        filepath,
        top_name,
        title,
        validate=not erc_loop,
        place_fields=place_fields,
    )


# Pin count above which auto_stub=True turns a net into labels.
AUTO_STUB_PINS = 3


def resolve_auto_stub(options):
    """Turn an ``auto_stub`` setting into the options the engine reads.

    ``auto_stub`` may be False (off), True (on, with the default threshold
    of AUTO_STUB_PINS), or an integer N: on, and a net with more than N pins
    becomes labels. The threshold applies to the net's pins on one sheet
    (``auto_stub_max_wire_pins``) and to all of them (``auto_stub_fanout``,
    which stubs nets with at least that many pins); either can still be set
    on its own and wins.

    Args:
        options (dict): Engine options. Updated in place: ``auto_stub``
            becomes a bool and the thresholds are filled in.

    Raises:
        ValueError: ``auto_stub`` is an integer less than 1, or not a bool or integer.
    """
    setting = options.get("auto_stub", False)
    if setting is None or setting is False:
        options["auto_stub"] = False
        return
    if setting is True:
        pins = AUTO_STUB_PINS
    elif isinstance(setting, int):
        if setting < 1:
            raise ValueError(
                f"auto_stub must be True, False or a pin count of at least 1, not {setting}."
            )
        pins = setting
    else:
        raise ValueError(f"auto_stub must be True, False or a pin count, not {setting!r}.")
    options["auto_stub"] = True
    options.setdefault("auto_stub_max_wire_pins", pins)
    options.setdefault("auto_stub_fanout", pins + 1)


def gen_schematic(
    circuit,
    filepath=".",
    top_name=get_script_name(),
    title="SKiDL-Generated Schematic",
    flatness=0.0,
    retries=2,
    **options,
):
    """Create a KiCad 9 schematic file from a Circuit object.

    Args:
        circuit (Circuit): The Circuit object that will have a schematic generated for it.
        filepath (str, optional): The directory where the schematic files are placed. Defaults to ".".
        top_name (str, optional): The name for the top of the circuit hierarchy. Defaults to get_script_name().
        title (str, optional): The title of the schematic. Defaults to "SKiDL-Generated Schematic".
        flatness (float, optional): Determines how much the hierarchy is flattened in the schematic.
            Defaults to 0.0 (completely hierarchical). Use 1.0 to flatten everything into one sheet.
        retries (int, optional): Number of times to re-try if routing fails. Defaults to 2.
        options (dict, optional): Dict of options and values, usually for drawing/debugging.
            Recognizes "seed": seed for the placer/router's random number
            generator. With a seed, the same circuit and options produce
            byte-identical output; without one (the default), placement is
            random and the drawing differs from run to run. Each retry derives
            its own stream from the seed, so retries still explore different
            layouts while the run as a whole stays reproducible.

    Auto-stub options (pass as keyword arguments):
        auto_stub (bool or int): Enable auto-stubbing for large/complex circuits. Converts nets that
            would fail routing into global labels, and runs a KiCad ERC correction loop to
            iteratively fix remaining issues. Power nets (GND, VCC, etc.) are automatically
            emitted as proper KiCad power symbols. An integer N turns it on and makes a
            net with more than N pins into labels; True means N = 3. Default False.
        auto_stub_fanout (int): Nets with at least this many pins are stubbed once
            placed. Default: N + 1.
        auto_stub_max_wire_pins (int): Max pins of a net on one sheet before it's
            stubbed after placement. Default: N.
        auto_stub_max_wire_dist (int): Max manhattan distance (mils) between pins before
            selective routing stubs the net. Default 2000.
        erc_max_iterations (int): Max ERC correction loop passes. Default 8.
        auto_stub_fallback (str): What to do when routing fails with auto_stub enabled.
            "labels" (default) — fall back to labels-only schematic.
            "raise" — raise the RoutingFailure so the caller sees it.
            "warn" — produce labels-only but also raise a warning exception.

    Tips for best results with auto_stub:
        - Use @subcircuit to group related parts (e.g. power supply, MCU, amplifier).
          Each subcircuit gets placed and routed independently, producing more wired
          connections and cleaner hierarchical sheets.
        - Keep subcircuits to 5-15 parts for best wire routing results.
        - Power nets are automatically detected and emitted as KiCad power symbols
          (e.g. power:GND, power:VCC) which display correctly in the schematic editor.

    Example::

        from skidl import *

        @subcircuit
        def power_supply(vin, vout, gnd):
            ldo = Part("Regulator_Linear", "AP2112K-3.3")
            ldo["VIN"] += vin
            ldo["VOUT"] += vout
            ldo["GND"] += gnd
            ldo["EN"] += vin
            for val in ["1uF", "1uF"]:
                c = Part("Device", "C", value=val)
                c[1] += vout if val == "1uF" else vin
                c[2] += gnd

        vcc = Net("VCC"); vcc.drive = POWER
        gnd = Net("GND"); gnd.drive = POWER
        vin = Net("VIN")

        power_supply(vin, vcc, gnd)

        generate_schematic(auto_stub=True)
    """

    from .._utils import active_logger
    from .place import PlacementFailure
    from .route import RoutingFailure
    from .sch_node import SchNode
    from . import tool_module

    _setup_kicad_env()
    resolve_auto_stub(options)

    # Part placement options that should always be turned on.
    options["use_push_pull"] = True
    options["rotate_parts"] = True
    options["pt_to_pt_mult"] = 5
    options["pin_normalize"] = True

    if options.get("auto_stub", False):
        # Phase 1: Heuristic auto-stubbing before first generation pass.
        auto_stub_nets(circuit, **options)
    elif options.get("power_symbols", True):
        # Draw power nets with power symbols rather than wires, as auto-stubbing does.
        power_nets = stub_power_nets(circuit, keep_drawn=True)
        active_logger.info(
            f"  [power_symbols] {', '.join(net.name for net in power_nets[:10])}"
            f"{'...' if len(power_nets) > 10 else ''}"
        )

    expansion_factor = 1.0
    failure_type = None

    # Seed the placer/router once for the whole run. Doing it here rather than
    # inside place()/route() matters: those recurse into child nodes, so seeding
    # there would restart every sibling sheet on the same stream.
    base_seed = options.get("seed")

    for attempt in range(retries):
        if base_seed is not None:
            # Give each attempt its own stream derived from the caller's seed.
            # Reusing the seed verbatim would make every retry re-explore the
            # same placement, which is the opposite of what a retry is for --
            # while deriving it keeps the whole run reproducible.
            rng_seed(f"{base_seed}:{attempt}")

        node = _new_node(
            circuit, tool_module, filepath, top_name, title, flatness, options
        )

        # Say which attempt this is once the first has failed.
        note = f"attempt {attempt + 1}/{retries}," if attempt else ""
        with progress.prefixed(note) if note else contextlib.nullcontext():
            try:
                _place_and_classify(node, circuit, expansion_factor, **options)
                if options.get("auto_stub", False) and options.get(
                    "label_clearance", False
                ):
                    # Label-clearance Pass 2: Pass 1's classifiers just froze the
                    # final stub set, but the placement they ran on reserved no room
                    # for the resulting net labels. Re-run preprocessing (recomputes
                    # each part's lbl_bbox to include the now-stubbed pins' label
                    # boxes) and rebuild the node (NetTerminals are derived from stub
                    # state, so the rebuild also clears stale terminals), then place
                    # WITHOUT reclassifying — classify=False keeps the stubs frozen.
                    # finalize first (as the ERC-regen path does before its rebuild):
                    # removes Pass-1 NetTerminals and clears force/bbox/lbl_bbox/tx so
                    # the rebuild starts clean. It does NOT touch pin.stub/net._stub,
                    # so the frozen stub set survives.
                    finalize_parts_and_nets(circuit, **options)
                    node = _new_node(
                        circuit, tool_module, filepath, top_name, title, flatness, options
                    )
                    _place_and_classify(
                        node, circuit, expansion_factor, classify=False, **options
                    )
                node.route(**options)

            except PlacementFailure as e:
                finalize_parts_and_nets(circuit, **options)
                failure_type = e
                active_logger.warning(
                    f"Placement failed on attempt {attempt + 1}/{retries}: {e}"
                )
                continue

            except RoutingFailure as e:
                finalize_parts_and_nets(circuit, **options)
                expansion_factor *= 1.5
                failure_type = e
                active_logger.warning(
                    f"Routing failed on attempt {attempt + 1}/{retries}, expanding area by 1.5x: {e}"
                )
                continue

        _snap_unwired_parts(node, options)

        # Serialize in the requested output format (KiCad s-expression or SVG).
        output_file = _write_output(
            circuit, node, filepath, top_name, title, options
        )

        active_logger.info(f"Schematic written to {output_file}")

        finalize_parts_and_nets(circuit, **options)

        # Phase 2: ERC correction loop (only when auto_stub is enabled).
        # kicad-cli can only check .kicad_sch, so this is skipped for SVG.
        if (
            options.get("auto_stub", False)
            and options.get("output_format", "kicad") == "kicad"
            and shutil.which("kicad-cli")
        ):
            max_erc_iterations = options.get("erc_max_iterations", 3)
            prev_errors = None
            for erc_attempt in range(max_erc_iterations):
                progress.report("checking with KiCad ERC")
                erc_report = _run_erc(output_file)
                errors = _parse_erc_report(erc_report)
                fixable = [e for e in errors if e[0] in FIXABLE_ERROR_TYPES]

                if not fixable:
                    if errors:
                        active_logger.warning(
                            f"KiCad ERC found {len(errors)} issues in {output_file}"
                        )
                    active_logger.info(
                        f"ERC clean after {erc_attempt + 1} iteration(s)"
                    )
                    break

                # Regenerating can't help if the last pass changed nothing, or if
                # there is nothing left to stub. Classification undoes ERC stubs
                # on deferred nets, so the second check alone wouldn't catch this.
                if sorted(errors) == prev_errors or not _stub_nets_for_erc_errors(
                    circuit, fixable
                ):
                    active_logger.warning(
                        f"KiCad ERC found {len(errors)} issues in {output_file}: "
                        f"{len(fixable)} unfixable errors remain after "
                        f"{erc_attempt + 1} iteration(s)"
                    )
                    break
                prev_errors = sorted(errors)

                active_logger.info(
                    f"ERC correction: stubbed nets for {len(fixable)} errors, regenerating (iteration {erc_attempt + 1})"
                )

                # Full regeneration — try with expansion before giving up.
                erc_regen_ok = False
                for erc_expansion in [1.0, 1.5, 2.25]:
                    try:
                        node = _new_node(
                            circuit,
                            tool_module,
                            filepath,
                            top_name,
                            title,
                            flatness,
                            options,
                        )
                        _place_and_classify(node, circuit, erc_expansion, **options)
                        node.route(**options)
                        _snap_unwired_parts(node, options)
                        output_file = _write_output(
                            circuit, node, filepath, top_name, title, options
                        )
                        finalize_parts_and_nets(circuit, **options)
                        erc_regen_ok = True
                        break
                    except (RoutingFailure, PlacementFailure) as inner_e:
                        finalize_parts_and_nets(circuit, **options)
                        if erc_expansion < 2.25:
                            active_logger.info(
                                f"ERC regeneration routing failed at {erc_expansion}x, "
                                f"trying {erc_expansion * 1.5}x expansion"
                            )
                        else:
                            active_logger.warning(
                                f"ERC regeneration routing failed after all expansion attempts: {inner_e}"
                            )

                if not erc_regen_ok:
                    # Routing failed even with expansion — handle per fallback policy.
                    _handle_fallback(
                        circuit,
                        tool_module,
                        filepath,
                        top_name,
                        title,
                        flatness,
                        options,
                        active_logger,
                        reason=f"ERC correction regeneration failed after expansion attempts",
                    )
                    break

        return

    # All retries exhausted.
    if failure_type and options.get("auto_stub", False):
        _handle_fallback(
            circuit,
            tool_module,
            filepath,
            top_name,
            title,
            flatness,
            options,
            active_logger,
            reason=f"Routing failed after all {retries} retries",
        )
        return

    finalize_parts_and_nets(circuit, **options)

    if failure_type:
        raise failure_type
    else:
        raise RuntimeError("Schematic generation failed for unknown reasons")
