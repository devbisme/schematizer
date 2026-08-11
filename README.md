# schematizer

Turn a **generic, hierarchical netlist** (JSON) into a KiCad schematic.

`schematizer` does the *graphical* half of EDA: it takes a tool-neutral netlist
document and does all the drawing work — component placement, wire routing, and
writing `.kicad_sch` files for a selected KiCad version (5–10). It is not tied
to any particular front end: **any** tool that emits the netlist format can use
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
```

## Library use

```python
import json
from schematizer import render

with open("netlist.json") as f:
    netlist = json.load(f)

render(netlist, tool="kicad9", filepath="out", top_name="mydesign")
```

`render()` also accepts a path to the JSON file directly.

## How it fits with SKiDL

To the SKiDL end user nothing changes: `Circuit.generate_schematic()` builds
the generic netlist internally and calls `render()` here. Because the seam is a
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

## Supported KiCad versions

`--tool` accepts `kicad5` … `kicad10`. KiCad 6–10 share one modern
S-expression writer (with a couple of KiCad-8 format toggles); KiCad 5 uses the
legacy EESCHEMA writer.
