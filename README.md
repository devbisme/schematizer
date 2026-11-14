![PyPI Version](https://img.shields.io/pypi/v/schematizer.svg)
![Python Versions](https://img.shields.io/pypi/pyversions/schematizer.svg)
![License](https://img.shields.io/pypi/l/schematizer.svg)
![Downloads](https://img.shields.io/pypi/dm/schematizer.svg)
![GitHub Stars](https://img.shields.io/github/stars/devbisme/schematizer.svg?style=social)
![GitHub Forks](https://img.shields.io/github/forks/devbisme/schematizer.svg?style=social)
![GitHub Issues](https://img.shields.io/github/issues/devbisme/schematizer.svg)
![GitHub Last Commit](https://img.shields.io/github/last-commit/devbisme/schematizer.svg)

# schematizer

By 2030, most electronic design won't be done with schematics.
Probably not even with SKiDL.
AI will handle it, going from natural language descriptions of systems
to netlists that specify the interconnections of components.

But people *will* still need to understand the system the AI has designed,
and schematics are the most natural way to do that.
And while a netlist tells you what's connected to what,
it doesn't tell you where anything goes on the page or how they are logically ordered.

`schematizer` does that work. You give it a **generic, hierarchical netlist** (a JSON
file) and it places the components, routes the wires and writes either editable
`.kicad_sch` files or static, cross-linked `.svg` pages.

It isn't tied to any particular front end. [SKiDL](https://github.com/devbisme/skidl)
is the first tool to feed it (SKiDL describes the electrical connections in code and
hands the netlist over), but the input is just JSON, so *anything* that emits the format
can use it.

The netlist (produced by `skidl.schematic_netlist.build_generic_netlist`, or whatever
you write that does the same job) carries each component's symbol definition inside it.
So this tool needs **no access to KiCad symbol libraries**. The netlist also carries a
few layout *hints*: how to mirror or rotate a component (`symtx`) and which nets to
stub (`stub`/`netio`).

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

A big design takes a while (200 parts in 10 seconds). So you're not left staring at a dead terminal, the command
shows which sheet it's placing or routing on a status line. `--no-progress` turns that
off, and `--progress` shows it even when the output is redirected.

## Library use

```python
import json
from schematizer import render

with open("netlist.json") as f:
    netlist = json.load(f)

render(netlist, tool="kicad9", filepath="out", top_name="mydesign")
render(netlist, tool="kicad9", filepath="out", format="svg")
```

You can also skip the `json.load()` and hand `render()` the path to the JSON file.

`render()` keeps quiet unless you ask. `progress=True` shows the same status line the
command does. Pass a function instead and it gets each message ("routing fpga") as a
string:

```python
render(netlist, filepath="out", progress=True)
render(netlist, filepath="out", progress=lambda msg: print(msg))
```

## How it fits with SKiDL

If you use SKiDL, nothing changes. `Circuit.generate_schematic()` and
`Circuit.generate_svg()` build the generic netlist internally and call `render()`
(with `format="kicad"` and `format="svg"`, respectively).

## Status

**Self-contained.** The placement/routing/writer engine is in
`schematizer/engine`. The netlist is rebuilt into a lightweight intermediate
representation (`schematizer.ir`: `SchPart`/`SchPin`/`SchNet`) that the engine consumes
directly. That means no `.kicad_sym` re-parse, no KiCad library access, and **no runtime
dependency on SKiDL**.
The KiCad power-symbol library (for `GND` and the supply rails) is kept in `schematizer/data`.

## Output formats

`--format kicad` (the default) writes editable `.kicad_sch` files. `--format svg`
writes static `.svg` pages instead:

```bash
schematizer netlist.json --tool kicad9 --format svg -o out
```

The same placement and routing pass is used for either output. They only differ in the last step,
where the result gets written out, so an SVG page has the same layout as the `.kicad_sch`
you would have gotten. Symbols are drawn from the graphics embedded in the netlist, so
the SVG output doesn't need KiCad libraries either.

`--flatness` controls the hierarchy for both formats:

- At `0.0` (the default), every subcircuit gets its own page.
- At `1.0`, everything collapses into a single page.
- Values in between flatten only the smaller subcircuits.

In the SVG output, a child appears on its parent's page as a **hyperlinked rectangle**,
and each child page links back to its parent. You can click
your way through a hierarchical design in a browser.

In the KiCad output, a net that leaves a subcircuit's page gets a hierarchical label at
its pins. The subcircuit's sheet symbol in the parent gets a matching sheet pin
with a label on it, so the connection runs through the hierarchy the way KiCad expects.
Power nets travel by power symbol instead. Stubbed nets (the ones marked `stub`, or
turned into labels when routing fails) travel by global label.

Each KiCad page uses the smallest A-size sheet whose frame holds the drawing above the
title block. An SVG page is sized to its drawing instead, since an SVG renders at
whatever size it declares.

## Labels-only mode

Sometimes you don't care what the schematic looks like. You just need one whose
connections are right, say as the input to a PCB layout flow. That's what
`mode="labels"` (CLI: `--mode labels`) is for. Its connectivity is exact by
construction:

- Parts are placed unrotated on a plain, deterministic grid. There's no placement
  optimization and no wire routing, so it's fast (about two seconds for a 190-part design).
- Every connected pin gets a short wire stub ending in one of three things: a power
  symbol (if the net name matches a KiCad power symbol), a global label (if the net is
  on more than one sheet) or a local label (if it's on one sheet). Pins on the
  no-connect net get a no-connect flag.
- Each subcircuit with parts becomes a sheet directly under the root sheet. (Nested
  subcircuits become sibling sheets.)

```bash
schematizer netlist.json --tool kicad10 --mode labels -o out --top-name mydesign
```

This is KiCad output only; `format="svg"` is rejected in this mode. And if you compare
connectivity afterward, leave out the `#` references (power symbols, PWR_FLAGs). KiCad
leaves them out of the netlists it exports.

## Optional netlist keys

These keys are additive. The schema stays at version 1, and readers ignore keys they
don't use:

- `components[].fields`: user part fields such as `MPN`. They're written as symbol
  properties.
- `nets[].attrs`: per-net attributes for layout tools, e.g.
  `{"netclasses": ["Power"]}`.
- `netclasses`: net class definitions referenced by `nets[].attrs`.
- A net with `"nc": true` is the no-connect net. Its pins get no-connect flags.

## Placement

A schematic that's electrically correct can still be miserable to read. So wired parts
are placed by the drafting conventions a person would follow:

- **Rails up, grounds down.** A pin on a supply net (`VCC`, `+3V3`, `VBAT`, ...) points
  up and a pin on a ground or negative supply (`GND`, `VSS`, `-12V`, ...) points down.
  That way decoupling caps and pull-ups stand upright.
- **Signals flow left to right.** A part isn't mirrored if that would put its inputs on
  the right or its outputs on the left, and each part goes on the side of the pin it's
  wired to.
- **ICs stay upright.** A part with more than three pins can be mirrored, but it isn't
  turned on its side.
- **Passives sit beside their pins.** A two-pin part that hangs off the circuit by one
  pin (a pull-up, a series resistor, an LED with its resistor) is fixed next to the pin
  it's wired to.
- **Power nets get power symbols.** Nets with power names are drawn with a power symbol
  on each pin instead of being wired across the sheet. So are nets with names of their
  own (`VBUS_SW`) that go to a power pin and at least three other pins; those get
  labels. A power net that already has a power symbol part of its own in the netlist is
  left wired, and it points the way its symbol does.
- **Labels go on pins, not out in space.** A one-pin net that leaves the sheet is labeled at
  its pin. If it has several pins, they're wired together and the
  label goes beside one of them, far enough out that the other wires have room to join
  it.
- **Unwired parts go in tidy rows.** Parts with no wires at all (a bank of decoupling
  caps whose pins all go to power symbols) are laid out in rows, with each rail's caps
  side by side and room for their references and values.
- **Text stays clear.** A part's reference and value go together on a side of the part
  with no pins, wherever they don't cover another part, a wire or other text. If a
  neighbor leaves no room for both, they're placed separately. The names of power
  symbols on neighboring pins of an IC are turned to read up the sheet, because side by
  side they'd run together into one long, meaningless word.

A part whose orientation is set in the netlist (`symtx`) keeps it. You said where you
wanted it, so that's where it goes.

**Routing feeds back into placement.** Once a sheet is routed, a wire that steps
sideways between two pins is telling you something: those pins are nearly, but not
quite, in line. So the part with the least wired to it is moved by the size of the
step (up to 3 grid steps) and the sheet is routed again. The new routing is kept only
if it has fewer bends and wire crossings. Otherwise the parts go back where they were.
On my 169-part test board, this removes about a quarter of those steps and 8% of the
bends for about 15% more time.

**The same circuit is drawn the same way.** When a subcircuit is used more than once
(e.g., the two channels of an amplifier), the sheet of the first one is placed and routed and
the others use its layout. Sheets match if their parts have the same symbols and
are wired the same. The values and net names can differ.

**A sheet can be laid out several times.** With `sheet_tries=3` (CLI:
`--sheet-tries 3`), each sheet is placed and routed three times and the best layout is
kept. Layouts are compared the way `tries` compares whole drawings: by wire length, with
each bend and wire crossing counted as 5 mm of wire and each overlap of parts or text
as 10 mm. The difference is that `tries` keeps the best whole drawing, while this keeps
the best of each sheet. On the 169-part test board, three tries remove 8% of the bends,
40% of the crossings and 6% of the wire. They also take about twice as long. (Nothing's
free.)

A named net that stays on one sheet is wired, but its name isn't shown. Routing a wire
out to a label makes a crowded sheet even harder to route. If you want those nets
labeled anyway, pass `label_local_nets=True`.

Don't like some of this? Four options turn parts of it off (CLI: `--no-preplace`,
`--no-power-symbols`, `--no-nudge`, `--no-reuse`):

```python
render(netlist, filepath="out", preplace=False)       # force-directed placement from random positions
render(netlist, filepath="out", power_symbols=False)  # wire the power nets
render(netlist, filepath="out", nudge=False)          # don't move parts after routing
render(netlist, filepath="out", reuse_layouts=False)  # lay out identical sheets separately
```

With `preplace=False`, groups of more than 20 wired parts are laid out in plain rows.
Force-directed placement is too slow for them.

With `auto_stub`, parts placed this way keep the short wires routed to them. Only nets
with many pins or far-apart pins become labels. (With `preplace=False`, auto-stubbing
works the way it used to: it labels every net of a small subcircuit and snaps two-pin
parts onto the pins they connect to.)

## Reproducible drawings

Unwired parts and the blocks of a sheet start at random positions, and the router
breaks ties randomly. So the same netlist normally gives you a different (but equally
valid) drawing on every run. Pass a seed to pin it down:

```bash
schematizer netlist.json --tool kicad9 --seed 42 -o out
```

```python
render(netlist, tool="kicad9", filepath="out", seed=42)
```

The same netlist, seed and options give byte-identical output, across separate
processes and for both formats. (There's one exception: the ERC report that kicad-cli
writes when `auto_stub` is on. Its first line is a timestamp.) Without a seed, nothing
changes: you get a random drawing each run.

Why would you want this?

- **Regression testing.** The drawing you got yesterday is the drawing you get today.
- **Bug reports.** You can point at one specific bad layout instead of describing it.
- **Re-rolling.** If you don't like a layout, try `--seed 1`, `2`, `3` until you get
  one you want. Then write the number down.

Retries still explore. When routing fails and the engine tries again with a larger
area, each attempt derives its own random stream from your seed. A retry is a different
layout, but the run as a whole stays reproducible.

Seeding is local to this library. It uses its own generator, so it neither disturbs nor
depends on the global state of the `random` module in your program.

## When routing fails

A crowded sheet doesn't always route on the first try. So the engine tries again at
three levels before it gives up:

1. If a net can't find a way through, the nets routed ahead of it are torn up and all
   of them are routed again with that net first (up to 4 times).
2. A sheet that still fails is placed again by itself with 25% more room around its
   parts and routed again (up to 6 times). This doesn't touch the other sheets, since
   the parent only shows it as a sheet symbol.
3. If that fails too, the whole design is placed and routed again with more room
   (`retries`, 2 by default).

Routing only counts as done if three things are true:

- Every wired pin has a wire of its net ending on it.
- No wire runs over the end of another net's pin.
- Each net's wires are in one piece.

Why so picky? KiCad connects a pin only to a wire that ends on it. A drawing that
breaks one of these rules would look right and be missing connections, which is the
worst kind of wrong.

If all three levels fail, the command line says so in one line and exits with status 1.
`render()` raises `RoutingFailure`.

### Auto-stubbing

Some nets just aren't worth wiring. `auto_stub` (CLI: `--auto-stub [PINS]`) draws the
ones that are hard to route with labels instead of wires. A net with more than `PINS`
pins, or one whose pins end up far apart, uses labels. Then KiCad's ERC is run on
the result (this needs `kicad-cli`) and the nets it still complains about become labels
too. If routing fails anyway, a labels-only schematic is written instead.

```bash
schematizer netlist.json -o out --auto-stub      # more than 3 pins -> labels
schematizer netlist.json -o out --auto-stub 6    # more than 6 pins -> labels
```

In Python, `auto_stub=True` means 3 and `auto_stub=6` sets the count. A lower count
gives you fewer wires and more labels. On the 169-part test board, the total wire
length is about 560 at 2, 1060 at 3, 1440 at 4 and 1940 at 10 (mm, averaged over 4
seeds). Without auto-stubbing it's 3180. Don't get greedy, though: at 2, the labels
start to overlap on smaller boards.

## Checking the result

A schematic can look fine and still be wired wrong. `check_netlist=True` (CLI:
`--check-netlist`) has KiCad read back the schematic that was written and compares its
connections with the input netlist:

```bash
schematizer netlist.json -o out --check-netlist
```

```python
from schematizer import NetlistMismatch, render

try:
    render(netlist, filepath="out", check_netlist=True)
except NetlistMismatch as e:
    print(e.problems)   # e.g. ["SDA is shorted to SCL", "EN is in 2 pieces (R4.1 not connected to the rest)"]
```

If they differ, `render()` raises `NetlistMismatch`. The command line prints the nets
that differ and exits with status 1. The files are left in place so you can look at
them.

A few things to know about the check:

- It needs `kicad-cli` and KiCad output.
- It takes about as long as KiCad needs to open the design (around a second for 170
  parts).
- It's off by default.
- With `tries`, only the layout that's kept is checked.
- It ignores `#` references (power symbols, flags), which KiCad leaves out of its
  netlists, and nets with fewer than two pins.

Nothing else runs KiCad on the output, apart from the ERC that auto-stubbing uses to
find nets to label.

## Supported KiCad versions

`--tool` accepts `kicad6` through `kicad10`. They share one modern S-expression writer
(with a couple of format toggles for KiCad 8). The version still matters for
`--format svg`, because the symbol graphics embedded in the netlist are flavored by
KiCad version.

KiCad 5 is **not** supported, and you'll get an error that says so. It uses the legacy
EESCHEMA `.sch` format instead of `.kicad_sch`, and its libraries describe symbols in a
different form (not S-expressions) that the generic netlist doesn't carry.

Sorry, KiCad 5. It was a good run.
