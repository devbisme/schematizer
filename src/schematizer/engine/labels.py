# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""
Labels-only schematic writer.

Writes KiCad schematics whose connectivity is exact by construction, for use
when correctness matters more than a readable drawing (e.g. as the input to a
PCB flow). There is no placement optimization and no wire routing:

- Each part unit is placed unrotated on a deterministic shelf-packed grid.
- Every connected pin gets a short wire stub ending in one of:
    * a power symbol, if the net name matches a KiCad power symbol;
    * a global label, if the net appears on more than one sheet;
    * a local label, if the net is confined to one sheet.
- Pins on the no-connect net get a no-connect flag.

KiCad then joins pins by label name, so no part-to-part wires are needed.

Hierarchy: every distinct ``hiertuple`` that holds parts becomes one sheet
directly under the root sheet (nested subcircuits become sibling sheets).
Parts at the top level (``hiertuple == ("",)``) go on the root sheet.

Coordinates here are KiCad sheet millimetres (y down). Symbol pin coordinates
from the netlist are symbol-local millimetres (y up), so a pin of a symbol
placed unrotated at (X, Y) sits at (X + pin.x, Y - pin.y).
"""

import os
from collections import defaultdict

from simp_sexp import Sexp

from ..geometry import Tx
from ..ir import NCNet
from . import sexp_schematic as sx

GRID = 1.27  # KiCad schematic connection grid (mm).
STUB = 2.54  # Length of the wire stub from a pin to its label (mm).
MARGIN = 20.0  # Page margin (mm).
FONT = 1.27  # Label/field font size (mm).

# Outward direction (sheet coords, y down) of a pin whose SKiDL orientation
# letter gives the direction from its connection point into the symbol body.
_OUTWARD = {"L": (1, 0), "R": (-1, 0), "U": (0, 1), "D": (0, -1)}

# Label angle so the text extends away from the pin, for each outward direction.
_LABEL_ANGLE = {(1, 0): 0, (-1, 0): 180, (0, -1): 90, (0, 1): 270}

# Outward direction -> the same direction as an angle in symbol (y up) degrees.
_OUTWARD_DEG = {(1, 0): 0, (0, -1): 90, (-1, 0): 180, (0, 1): 270}


def _snap(v):
    return round(round(v / GRID) * GRID, 4)


def _text_len(text):
    """Rough rendered length (mm) of label text at FONT size, plus the label arrow."""
    return 0.8 * FONT * len(text) + 2.0


def _units(part):
    """[(unit_obj, unit_num, pins)] for a part, in unit order."""
    if not part.unit:
        return [(part, 1, list(part.pins))]
    units = sorted(part.unit.values(), key=lambda u: u.num)
    common = [p for u in units if u.num == 0 for p in u.pins]
    return [(u, u.num, list(u.pins) + common) for u in units if u.num != 0]


def _graphic_points(cmds):
    """Symbol-local (x, y) points from drawing commands (for the body bbox)."""
    pts = []

    def walk(node):
        if not isinstance(node, list) or not node:
            return
        if node[0] in ("at", "start", "end", "xy", "center", "mid") and len(node) >= 3:
            try:
                pts.append((float(node[1]), float(node[2])))
            except (TypeError, ValueError):
                pass
            return
        if node[0] in ("property", "text"):
            return
        for sub in node[1:]:
            walk(sub)

    for cmd in cmds:
        walk(cmd)
    return pts


def _pin_dir(pin):
    orient = pin.orientation
    if isinstance(orient, int):
        # KiCad pin angle (into the body, y up) -> SKiDL letter.
        orient = {0: "R", 90: "U", 180: "L", 270: "D"}[orient % 360]
    return _OUTWARD[orient]


def _net_name(pin):
    net = pin.net
    if net is None:
        return None
    return net.name


class _Item:
    """One placed part unit."""

    def __init__(self, part, unit_obj, unit_num, pins):
        self.part = part
        self.unit_obj = unit_obj
        self.unit_num = unit_num
        self.pins = pins
        cmds = list(part.draw_cmds.get(unit_num, [])) + list(part.draw_cmds.get(0, []))
        pts = _graphic_points(cmds) + [(p.x, p.y) for p in pins]
        if not pts:
            pts = [(-2.54, -2.54), (2.54, 2.54)]
        xs, ys = [p[0] for p in pts], [p[1] for p in pts]
        x0, y0, x1, y1 = min(xs), min(ys), max(xs), max(ys)
        # Room for stub + label (or power symbol) on each side the pins exit.
        m = {(1, 0): 2.54, (-1, 0): 2.54, (0, 1): 2.54, (0, -1): 2.54}
        for pin in pins:
            name = _net_name(pin)
            d = _pin_dir(pin)
            need = STUB + (_text_len(name) if name else 2.0)
            m[d] = max(m[d], need)
        # Extents relative to the symbol origin, in sheet coords (y down).
        self.ext = (
            x0 - m[(-1, 0)],
            -y1 - m[(0, -1)] - 2.54,  # room for the Reference field above
            x1 + m[(1, 0)],
            -y0 + m[(0, 1)] + 2.54,  # room for the Value field below
        )
        self.w = self.ext[2] - self.ext[0]
        self.h = self.ext[3] - self.ext[1]
        self.origin = (0.0, 0.0)


def _pack(items):
    """Shelf-pack items onto the smallest A-size page that holds them."""
    sizes = list(sx.A_SIZES.items())
    area = sum(it.w * it.h for it in items)
    widest = max((it.w for it in items), default=0)
    idx = 0
    for idx, (name, (pw, ph)) in enumerate(sizes):
        if area * 1.6 < (pw - 2 * MARGIN) * (ph - 2 * MARGIN) and widest < pw - 2 * MARGIN:
            break
    while True:
        name, (pw, ph) = sizes[idx]
        x, y, shelf_h = MARGIN, MARGIN + 5.0, 0.0
        for it in items:
            if x + it.w > pw - MARGIN and x > MARGIN:
                x, y, shelf_h = MARGIN, y + shelf_h, 0.0
            it.origin = (_snap(x - it.ext[0]), _snap(y - it.ext[1]))
            x += it.w
            shelf_h = max(shelf_h, it.h)
        if y + shelf_h <= ph - MARGIN or idx == len(sizes) - 1:
            # The largest page may still overflow; KiCad accepts items off-page.
            return name
        idx += 1


class _Writer:
    def __init__(self, circuit, filepath, top_name, title):
        self.circuit = circuit
        self.filepath = filepath
        self.project = top_name
        self.title = title
        self.root_uuid = sx._gen_uuid(f"labels:{top_name}:root")
        self.pwr_count = 0

    def uid(self, *names):
        return sx._gen_uuid("labels:" + ":".join(str(n) for n in (self.project,) + names))

    # -- sheets --------------------------------------------------------------

    def sheets(self):
        """{hiertuple: [parts]} with sheet names; root holds top-level parts."""
        by_sheet = defaultdict(list)
        for part in self.circuit.parts:
            by_sheet[tuple(part.hiertuple)].append(part)
        names = {}
        used = set()
        for ht in sorted(by_sheet):
            if ht == ("",):
                continue
            levels = [h for h in ht if h]
            name = levels[-1] if levels else "sheet"
            if name in used:
                name = "_".join(levels)
            used.add(name)
            names[ht] = name
        return by_sheet, names

    # -- elements --------------------------------------------------------------

    def stub_elements(self, it, sheet_key, net_sheets, uuid_path):
        """Stub wires, labels, power symbols and no-connect flags for one unit."""
        out = []
        X, Y = it.origin
        ref = it.part.ref
        for pin in it.pins:
            px, py = _snap(X + pin.x), _snap(Y - pin.y)
            key = (ref, it.unit_num, pin.num)
            net = pin.net
            if net is None:
                continue  # Unconnected pin: left as-is so ERC reports it.
            if isinstance(net, NCNet):
                out.append(
                    Sexp(["no_connect", ["at", px, py], ["uuid", self.uid("nc", *key)]])
                )
                continue
            dx, dy = _pin_dir(pin)
            ex, ey = _snap(px + STUB * dx), _snap(py + STUB * dy)
            out.append(
                Sexp(
                    [
                        "wire",
                        ["pts", ["xy", px, py], ["xy", ex, ey]],
                        ["stroke", ["width", 0], ["type", "default"]],
                        ["uuid", self.uid("wire", *key)],
                    ]
                )
            )
            name = net.name
            if name in sx.pwr_symbol_names:
                self.pwr_count += 1
                angle = (_OUTWARD_DEG[(dx, dy)] - sx._power_symbol_pin_angle(name)) % 360
                out.append(
                    sx.power_symbol_instance_sexp(
                        name,
                        ex,
                        ey,
                        angle,
                        f"#PWR{self.pwr_count:03d}",
                        uuid_path,
                        uuid_seed=f"labels:{self.project}:" + ":".join(map(str, key)),
                    )
                )
                continue
            angle = _LABEL_ANGLE[(dx, dy)]
            justify = "left" if angle in (0, 90) else "right"
            if len(net_sheets[name]) > 1:
                out.append(
                    Sexp(
                        [
                            "global_label",
                            name,
                            ["shape", "bidirectional"],
                            ["at", ex, ey, angle],
                            ["fields_autoplaced", "yes"],
                            ["effects", ["font", ["size", FONT, FONT]], ["justify", justify]],
                            ["uuid", self.uid("glabel", *key)],
                        ]
                    )
                )
            else:
                out.append(
                    Sexp(
                        [
                            "label",
                            name,
                            ["at", ex, ey, angle],
                            ["fields_autoplaced", "yes"],
                            [
                                "effects",
                                ["font", ["size", FONT, FONT]],
                                ["justify", justify, "bottom"],
                            ],
                            ["uuid", self.uid("label", *key)],
                        ]
                    )
                )
        return out

    def sheet_block(self, name, fname, sheet_uuid, page, index):
        x = _snap(25.4 + (index % 4) * 63.5)
        y = _snap(38.1 + (index // 4) * 38.1)
        w, h = 50.8, 25.4
        return Sexp(
            [
                "sheet",
                ["at", x, y],
                ["size", w, h],
                ["exclude_from_sim", "no"],
                ["in_bom", "yes"],
                ["on_board", "yes"],
                ["dnp", "no"],
                ["fields_autoplaced", "yes"],
                ["stroke", ["width", 0.1524], ["type", "solid"]],
                ["fill", ["color", 0, 0, 0, 0.0]],
                ["uuid", sheet_uuid],
                [
                    "property",
                    "Sheetname",
                    name,
                    ["at", x, _snap(y - 1.27), 0],
                    ["effects", ["font", ["size", FONT, FONT]], ["justify", "left", "bottom"]],
                ],
                [
                    "property",
                    "Sheetfile",
                    fname,
                    ["at", x, _snap(y + h + 1.27), 0],
                    ["effects", ["font", ["size", FONT, FONT]], ["justify", "left", "top"]],
                ],
                [
                    "instances",
                    [
                        "project",
                        self.project,
                        ["path", f"/{self.root_uuid}", ["page", str(page)]],
                    ],
                ],
            ]
        )

    def write_sheet(self, fname, sheet_uuid, title, parts, net_sheets, uuid_path, sheet_key, extra=()):
        items = []
        for part in sorted(parts, key=lambda p: (-len(p.pins), str(p.ref))):
            for unit_obj, unit_num, pins in _units(part):
                items.append(_Item(part, unit_obj, unit_num, pins))
        paper = _pack(items) if items else "A4"

        lib_symbols = {}
        elements = []
        for it in items:
            lib_id = f"{it.part.lib.filename}:{it.part.name}"
            lib_symbols.setdefault(lib_id, it.part)
            it.unit_obj.tx = Tx(dx=it.origin[0], dy=it.origin[1])
            elements.append(sx.part_to_sexp(it.unit_obj, uuid_path, Tx()))
            elements.extend(self.stub_elements(it, sheet_key, net_sheets, uuid_path))

        schematic = Sexp(
            [
                "kicad_sch",
                ["version", sx.file_version()],
                ["generator", "schematizer"],
                ["generator_version", sx.__version__],
                ["uuid", sheet_uuid],
                ["paper", paper],
            ]
        )
        schematic.append(Sexp(sx.create_title_block_sexp(title)))
        lib_sexp = Sexp(["lib_symbols"])
        for lib_id in sorted(lib_symbols):
            lib_sexp.append(Sexp(sx.part_to_lib_symbol_definition(lib_symbols[lib_id])))
        for pwr_lib_id in sorted(sx._power_lib_ids_in_elements(elements)):
            if pwr_lib_id not in lib_symbols:
                pwr = sx._extract_power_lib_symbol(pwr_lib_id.split(":", 1)[1])
                if pwr:
                    lib_sexp.append(pwr)
        schematic.append(lib_sexp)
        for elem in list(elements) + list(extra):
            schematic.append(elem)
        if uuid_path == f"/{self.root_uuid}":
            schematic.append(Sexp(["sheet_instances", ["path", "/", ["page", "1"]]]))
        schematic.append(Sexp(["embedded_fonts", "no"]))
        path = os.path.join(self.filepath, fname)
        sx._write_sexp_schematic(schematic, path)
        # Leave part transforms as the engine expects them.
        for it in items:
            it.unit_obj.tx = Tx()
        return path

    def write(self):
        by_sheet, names = self.sheets()

        # Which sheets each net appears on (decides global vs local labels).
        net_sheets = defaultdict(set)
        for ht, parts in by_sheet.items():
            for part in parts:
                for pin in part.pins:
                    if pin.net is not None and not isinstance(pin.net, NCNet):
                        net_sheets[pin.net.name].add(ht)

        blocks = []
        for index, ht in enumerate(sorted(names, key=lambda h: names[h])):
            name = names[ht]
            fname = f"{self.project}_{name}.kicad_sch"
            sheet_uuid = self.uid("sheet", *ht)
            self.write_sheet(
                fname,
                sheet_uuid,
                f"{self.title} - {name}",
                by_sheet[ht],
                net_sheets,
                f"/{self.root_uuid}/{sheet_uuid}",
                ht,
            )
            blocks.append(self.sheet_block(name, fname, sheet_uuid, index + 2, index))

        return self.write_sheet(
            f"{self.project}.kicad_sch",
            self.root_uuid,
            self.title,
            by_sheet.get(("",), []),
            net_sheets,
            f"/{self.root_uuid}",
            ("",),
            extra=blocks,
        )


def write_labels_schematic(circuit, filepath, top_name, title):
    """Write a labels-only KiCad schematic for a circuit.

    Args:
        circuit: IR Circuit (from schematizer.loader.load_netlist).
        filepath: Output directory.
        top_name: Base name of the top sheet and the KiCad project name.
        title: Title block text.

    Returns:
        str: Path of the top-level .kicad_sch file.
    """
    sx.init_power_symbol_data()
    sx.PROJECT_NAME = top_name
    os.makedirs(filepath, exist_ok=True)
    return _Writer(circuit, filepath, top_name, title).write()
