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

## Randomized layout

Placement starts parts at random positions and the router breaks ties randomly,
so the same netlist produces a different — equally valid — drawing on every
run. There is no way to pin a layout down: if you get one you want to keep,
keep the generated files.

## Supported KiCad versions

`--tool` accepts `kicad6` … `kicad10`, which share one modern S-expression
writer (with a couple of KiCad-8 format toggles). The version still matters for
`--format svg`, because the embedded symbol graphics are KiCad-version flavored.

KiCad 5 is **not** supported and is rejected with an explanatory error. It
needs the legacy EESCHEMA `.sch` format rather than `.kicad_sch`, and KiCad 5
libraries describe symbols in a different, non-s-expression form that the
generic netlist does not carry.
