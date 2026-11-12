![PyPI Version](https://img.shields.io/pypi/v/schematizer.svg)
![Python Versions](https://img.shields.io/pypi/pyversions/schematizer.svg)
![License](https://img.shields.io/pypi/l/schematizer.svg)
![Downloads](https://img.shields.io/pypi/dm/schematizer.svg)
![GitHub Stars](https://img.shields.io/github/stars/devbisme/schematizer.svg?style=social)
![GitHub Forks](https://img.shields.io/github/forks/devbisme/schematizer.svg?style=social)
![GitHub Issues](https://img.shields.io/github/issues/devbisme/schematizer.svg)
![GitHub Last Commit](https://img.shields.io/github/last-commit/devbisme/schematizer.svg)

# schematizer

Turn a **generic, hierarchical netlist** (JSON) into a KiCad schematic or an SVG drawing of one.

`schematizer` does the *graphical* half of EDA: it takes a tool-neutral netlist
document and does all the drawing work — component placement, wire routing, and
writing either editable `.kicad_sch` files or static, cross-linked `.svg` pages
for a selected KiCad version (6–10). It is not tied to any particular front end: **any** tool that emits the netlist format can use
it. [SKiDL](https://github.com/devbisme/skidl) is the first such producer —
SKiDL describes electrical interconnection in code and hands the netlist to
`schematizer` — but the input is just JSON, so other generators work too.

The netlist document (produced by
`skidl.schematic_netlist.build_generic_netlist`, or any equivalent emitter)
embeds each component's symbol definition and carries layout *hints* —
component mirror/rotate (`symtx`) and net stubbing (`stub`/`netio`). Because
symbols are embedded, this tool needs **no access to KiCad symbol libraries**.

## Install

```bash
pip install schematizer
```

## Command-line use

```bash
# netlist.json -> KiCad 9 schematic files in ./out/
schematizer netlist.json --tool kicad9 -o out --top-name mydesign

# ...or a set of linked SVG pages instead
schematizer netlist.json --tool kicad9 -o out --top-name mydesign --format svg
```

## Library use

```python
import json
from schematizer import render

with open("netlist.json") as f:
    netlist = json.load(f)

render(netlist, tool="kicad9", filepath="out", top_name="mydesign")
render(netlist, tool="kicad9", filepath="out", format="svg")
```

`render()` also accepts a path to the JSON file directly.

## How it fits with SKiDL

To the SKiDL end user nothing changes: `Circuit.generate_schematic()` and
`Circuit.generate_svg()` build the generic netlist internally and call `render()`
here (with `format="kicad"` and `format="svg"` respectively). Because the seam is a
plain JSON document, `schematizer` can be iterated and improved independently of
SKiDL — and reused by any other front end that emits the same format.

## Status

**Self-contained.** The placement/routing/writer engine is vendored under
`schematizer/engine`, and the netlist is reconstructed into a lightweight
intermediate representation (`schematizer.ir`: `SchPart`/`SchPin`/`SchNet`) that
the engine consumes directly — no `.kicad_sym` re-parse, no KiCad library
access, and **no runtime dependency on skidl**. The only runtime dependency is
`simp_sexp`. The KiCad power-symbol library (for `GND`/rail nets) is vendored in
`schematizer/data`.

## Output formats

`--format kicad` (the default) writes editable `.kicad_sch` files.
`--format svg` writes static `.svg` pages instead:

```bash
schematizer netlist.json --tool kicad9 --format svg -o out
```

Both come off the same placement and routing pass and differ only in the final
serialization, so an SVG page has the layout of the `.kicad_sch` it would have
produced. Symbols are drawn from the graphics embedded in the netlist, so SVG
output needs no KiCad libraries either.

`--flatness` controls hierarchy for both formats. At `0.0` (the default) every
subcircuit gets its own page; in the SVG output a child appears on its parent's
page as a **hyperlinked rectangle**, and each child page carries links back to
its parent and to the top sheet, so a hierarchical design can be clicked
through in a browser. At `1.0` everything collapses onto one page, and values
in between flatten only the smaller subcircuits.

Each SVG page is sized to its drawing rather than to an A-size sheet, since an
SVG renders at whatever size it declares.

## Labels-only mode

`mode="labels"` (CLI: `--mode labels`) writes a schematic whose connectivity is
exact by construction, for flows where correctness matters more than a
readable drawing (for example, as the input to a PCB layout flow):

- Parts are placed unrotated on a plain, deterministic grid. There is no
  placement optimization and no wire routing, so it is fast (about 2 s for a
  190-part design).
- Every connected pin gets a short wire stub ending in a power symbol (if the
  net name matches a KiCad power symbol), a global label (net on more than one
  sheet) or a local label (net on one sheet). Pins on the no-connect net get a
  no-connect flag.
- Each subcircuit with parts becomes a sheet directly under the root sheet
  (nested subcircuits become sibling sheets).

```bash
schematizer netlist.json --tool kicad10 --mode labels -o out --top-name mydesign
```

KiCad output only; `format="svg"` is rejected in this mode. KiCad leaves `#`
references (power symbols, PWR_FLAGs) out of its exported netlists, so compare
connectivity without them.

## Optional netlist keys

These keys are additive (the schema stays at version 1, and readers ignore
keys they don't use):

- `components[].fields`: user part fields such as `MPN`; written as symbol
  properties.
- `nets[].attrs`: per-net attributes for layout tools, e.g.
  `{"netclasses": ["Power"]}`.
- `netclasses`: net class definitions referenced by `nets[].attrs`.
- A net with `"nc": true` is the no-connect net; its pins get no-connect flags.

## Placement

Wired parts are placed by the drafting conventions a person would follow:

- **Rails up, grounds down.** A pin on a supply net (`VCC`, `+3V3`, `VBAT`, ...)
  points up and a pin on a ground or negative supply (`GND`, `VSS`, `-12V`, ...)
  points down, so decoupling caps and pull-ups stand upright.
- **Signals flow left to right.** Parts aren't mirrored if that would put their
  inputs on the right or their outputs on the left, and each part goes on the
  side of the pin it's wired to.
- **ICs stay upright.** A part with more than three pins can be mirrored but
  isn't turned on its side.
- **Passives sit beside their pins.** A two-pin part that hangs off the circuit
  by one pin (a pull-up, a series resistor, an LED with its resistor) is fixed
  next to the pin it's wired to.
- **Power nets get power symbols.** Nets with power names are drawn with a power
  symbol on each pin instead of being wired across the sheet. So are nets with
  names of their own (`VBUS_SW`) that go to a power pin and at least three other
  pins; they get labels. A power net that already has a power symbol part of
  its own in the netlist is left wired, and points the way its symbol does.

- **Labels go on pins, not out in space.** A net that leaves the sheet is
  labeled at its pin if it has only one there. If it has several, they're
  wired together and the label goes beside one of them, far enough out that
  the other wires have room to join it.
- **Unwired parts go in tidy rows.** Parts with no wires at all (a bank of
  decoupling caps whose pins all go to power symbols) are laid out in rows,
  with each rail's caps side by side and room for their reference and value.

A part whose orientation is set in the netlist (`symtx`) keeps it.

A named net that stays on one sheet is wired but its name isn't shown, since
routing a wire to a label makes a crowded sheet harder to route. Pass
`label_local_nets=True` to label those nets too.

Two options turn parts of this off (CLI: `--no-preplace`, `--no-power-symbols`):

```python
render(netlist, filepath="out", preplace=False)       # force-directed placement from random positions
render(netlist, filepath="out", power_symbols=False)  # wire the power nets
```

With `preplace=False`, groups of more than 20 wired parts are laid out in plain
rows, since force-directed placement is too slow for them.

With `auto_stub`, parts placed this way keep the short wires routed to them.
Only nets with many pins or far-apart pins become labels. (With
`preplace=False`, auto-stubbing works as before: it labels every net of a small
subcircuit and snaps two-pin parts onto the pins they connect to.)

## Reproducible drawings

Unwired parts and the blocks of a sheet start at random positions, and the
router breaks ties randomly, so the same netlist normally produces a different
— equally valid — drawing on every run. Pass a seed to pin it down:

```bash
schematizer netlist.json --tool kicad9 --seed 42 -o out
```

```python
render(netlist, tool="kicad9", filepath="out", seed=42)
```

The same netlist, seed and options give byte-identical output, across separate
processes and for both formats. (The one exception is the ERC report kicad-cli
writes when `auto_stub` is on: its first line is a timestamp.) Without a seed
the behavior is unchanged: random each run.

This is useful for regression-testing a drawing, for reporting a bug against a
specific bad layout, and for re-rolling a layout you don't like — try `--seed 1`,
`2`, `3` until you get one you want to keep, then record it.

Retries still explore: when routing fails and the engine retries with a larger
area, each attempt derives its own stream from your seed, so a retry is a
genuinely different layout while the run as a whole stays reproducible.

## When routing fails

A crowded sheet doesn't always route on the first try, so the engine tries
again at three levels before giving up:

1. If a net can't find a way through, the nets routed ahead of it are torn up
   and all are routed again with that net first (up to 4 times).
2. A sheet that still fails is placed again by itself with 25% more room
   around its parts and routed again (up to 6 times). This doesn't touch the
   other sheets, since its parent only shows it as a sheet symbol.
3. If that fails too, the whole design is placed and routed again with more
   room (`retries`, 2 by default).

Routing only counts as done if every wired pin has a wire of its net ending
on it, no wire runs over the end of another net's pin, and each net's wires
are in one piece. KiCad connects a pin only to a wire that ends on it, so a
drawing that breaks one of these would look right but be missing connections.

Seeding is local to this library. It uses its own generator, so it neither
disturbs nor depends on the `random` module's global state in the calling
program.

## Supported KiCad versions

`--tool` accepts `kicad6` … `kicad10`, which share one modern S-expression
writer (with a couple of KiCad-8 format toggles). The version still matters for
`--format svg`, because the embedded symbol graphics are KiCad-version flavored.

KiCad 5 is **not** supported and is rejected with an explanatory error. It
needs the legacy EESCHEMA `.sch` format rather than `.kicad_sch`, and KiCad 5
libraries describe symbols in a different, non-s-expression form that the
generic netlist does not carry.
