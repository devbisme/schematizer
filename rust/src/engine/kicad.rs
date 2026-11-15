//! Turn a placed and routed tree of nodes into .kicad_sch files.

use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::geometry::{pyround, round_to, BBox, Point, Segment, Tx};
use crate::ir::{FieldLayout, FieldSpot, NetId, NodeId, Part, PartId, Pin, PinId};
use crate::quality::{self as q, Box4, InstanceMap};
use crate::sexp::{self, Sexp};
use crate::sx;
use crate::uuid::gen_uuid;

use super::{Design, Res};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const MILS_TO_MM: f64 = 0.0254;

/// Distance from a power symbol's pin to its Value text.
pub const POWER_VALUE_DIST_MM: f64 = 3.81;
/// Distance from a power symbol's pin to where its upright Value text starts.
pub const POWER_VALUE_START_MM: f64 = 3.2;
/// Clearance between a symbol body and its Reference/Value text.
pub const FIELD_GAP_MM: f64 = 0.9;
/// Center-to-center spacing of stacked Reference/Value text.
pub const FIELD_PITCH_MM: f64 = 1.905;

/// Room the drawing must leave on the page: KiCad's frame on every side, and a
/// strip along the bottom for the title block (mm).
pub const FRAME_MARGIN_MM: f64 = 13.0;
pub const TITLE_BLOCK_H_MM: f64 = 33.0;

pub const A_SIZES: [(&str, (f64, f64)); 5] = [
    ("A4", (297.0, 210.0)),
    ("A3", (420.0, 297.0)),
    ("A2", (594.0, 420.0)),
    ("A1", (841.0, 594.0)),
    ("A0", (1189.0, 841.0)),
];

/// State the writers share while a design is written.
#[derive(Clone, Debug, Default)]
pub struct WriterState {
    /// Project name written into symbol instances; KiCad looks instances up by it.
    pub project_name: String,
    /// Instance path of the sheet whose elements are being emitted.
    pub sheet_uuid_path: String,
    /// Number of power symbols emitted, for their references.
    pub pwr_counter: usize,
}

static POWER_LIB_TEXT: &str = include_str!("../../../src/schematizer/data/power.kicad_sym");

/// The power symbols (GND, +3V3, ...) by name.
pub fn power_lib() -> &'static HashMap<String, Sexp> {
    static LIB: OnceLock<HashMap<String, Sexp>> = OnceLock::new();
    LIB.get_or_init(|| {
        let mut symbols = HashMap::new();
        if let Some(lib) = Sexp::parse(POWER_LIB_TEXT) {
            for sym in lib.fields("symbol") {
                symbols.insert(sym.str_at(1).to_string(), sym.clone());
            }
        }
        symbols
    })
}

/// True if a net with this name is drawn with a KiCad power symbol.
pub fn is_power_symbol_name(name: &str) -> bool {
    power_lib().contains_key(name)
}

/// S-expression file version for a target tool.
pub fn file_version(tool: &str) -> i64 {
    match tool {
        "kicad9" | "kicad10" => 20250114,
        _ => 20230409,
    }
}

pub fn round_mm(v: f64) -> f64 {
    round_to(v, 2)
}

/// The lib_symbol definition of a power symbol, named "power:NAME" to match its instances.
fn extract_power_lib_symbol(name: &str) -> Option<Sexp> {
    let mut sym = power_lib().get(name)?.clone();
    if let Some(v) = sym.list_mut() {
        v[1] = Sexp::A(format!("power:{}", name));
    }
    Some(sym)
}

/// The angle of a power symbol's pin: the direction its body extends at instance angle 0.
pub fn power_symbol_pin_angle(net_name: &str) -> f64 {
    let Some(sym) = power_lib().get(net_name) else { return 270.0 };
    for sub in sym.fields("symbol") {
        for pin in sub.fields("pin") {
            if let Some(at) = pin.field("at") {
                return at.num(3).rem_euclid(360.0);
            }
        }
    }
    for pin in sym.fields("pin") {
        if let Some(at) = pin.field("at") {
            return at.num(3).rem_euclid(360.0);
        }
    }
    270.0
}

/// The effects of a symbol field.
fn field_effects(justify: Option<&str>, hide: bool) -> Sexp {
    let mut effects = sx!["effects", sx!["font", sx!["size", 1.27, 1.27]]];
    if let Some(j) = justify {
        effects.push(sx!["justify", j]);
    }
    if hide {
        effects.push(sx!["hide", "yes"]);
    }
    effects
}

fn hidden_property(name: &str, value: &str, x: f64, y: f64, angle: f64) -> Sexp {
    sx!["property", name, value, sx!["at", x, y, angle], field_effects(None, true)]
}

fn wire_sexp(x1: f64, y1: f64, x2: f64, y2: f64, uuid_seed: &str) -> Sexp {
    sx![
        "wire",
        sx!["pts", sx!["xy", x1, y1], sx!["xy", x2, y2]],
        sx!["stroke", sx!["width", 0], sx!["type", "default"]],
        sx!["uuid", gen_uuid(uuid_seed)]
    ]
}

fn junction_sexp(x: f64, y: f64, uuid_seed: &str) -> Sexp {
    sx!["junction", sx!["at", x, y], sx!["diameter", 0], sx!["color", 0, 0, 0, 0], sx!["uuid", gen_uuid(uuid_seed)]]
}

fn f(n: f64) -> String {
    sexp::fmt_num(n)
}

/// A power-symbol instance (`power:<net_name>`) at (x, y) in mm.
pub fn power_symbol_instance_sexp(
    project: &str,
    net_name: &str,
    x: f64,
    y: f64,
    angle: f64,
    ref_: &str,
    uuid_path: &str,
    uuid_seed: &str,
) -> Sexp {
    // The Value text goes past the end of the symbol body, which extends from
    // the connection point in this direction (angles are counter-clockwise,
    // sheet Y points down).
    let out_angle = (angle + power_symbol_pin_angle(net_name)).rem_euclid(360.0);
    let out_x = pyround(out_angle.to_radians().cos());
    let out_y = -pyround(out_angle.to_radians().sin());
    let val_x = round_mm(x + out_x * POWER_VALUE_DIST_MM);
    let val_y = round_mm(y + out_y * POWER_VALUE_DIST_MM);
    let val_just = if out_x == 1.0 {
        Some("left")
    } else if out_x == -1.0 {
        Some("right")
    } else {
        None
    };

    sx![
        "symbol",
        sx!["lib_id", format!("power:{}", net_name)],
        sx!["at", x, y, angle],
        sx!["unit", 1],
        sx!["exclude_from_sim", "yes"],
        sx!["in_bom", "no"],
        sx!["on_board", "yes"],
        sx!["dnp", "no"],
        sx!["fields_autoplaced", "yes"],
        sx!["uuid", gen_uuid(&format!("pwr:{}", uuid_seed))],
        sx!["property", "Reference", ref_, sx!["at", x, y - 1.27, 0], field_effects(None, true)],
        sx!["property", "Value", net_name, sx!["at", val_x, val_y, 0], field_effects(val_just, false)],
        hidden_property("Footprint", "", x, y, 0.0),
        hidden_property("Datasheet", "", x, y, 0.0),
        sx!["pin", sexp::q("1"), sx!["uuid", gen_uuid(&format!("pwr_pin:{}", uuid_seed))]],
        sx!["instances", sx!["project", project, sx!["path", uuid_path, sx!["reference", ref_], sx!["unit", 1]]]]
    ]
}

/// Width and height (mm) of the part of a page the drawing can use.
fn usable_area(size: (f64, f64)) -> (f64, f64) {
    (size.0 - 2.0 * FRAME_MARGIN_MM, size.1 - 2.0 * FRAME_MARGIN_MM - TITLE_BLOCK_H_MM)
}

/// The smallest A-size paper whose frame holds a box (mils) above the title block.
pub fn pick_paper_size(bbox: &BBox) -> usize {
    let dim = |v: f64| if v.is_finite() { v.abs() * MILS_TO_MM } else { 0.0 };
    let (w_mm, h_mm) = (dim(bbox.w()), dim(bbox.h()));
    for (i, (_, size)) in A_SIZES.iter().enumerate() {
        let (uw, uh) = usable_area(*size);
        if w_mm <= uw && h_mm <= uh {
            return i;
        }
    }
    A_SIZES.len() - 1
}

/// The transformation that puts a node's circuitry on its sheet, and the sheet's paper.
///
/// It flips Y (placement is Y-up, KiCad is Y-down), converts mils to mm and
/// centers the drawing in the area above the title block.
pub fn calc_sheet_tx(bbox: &BBox) -> (Tx, &'static str) {
    let (paper, (pw, ph)) = A_SIZES[pick_paper_size(bbox)];
    let page_bbox = *bbox * Tx::lin(MILS_TO_MM, 0.0, 0.0, -MILS_TO_MM);
    let page_ctr = Point::new(pw / 2.0, (ph - TITLE_BLOCK_H_MM) / 2.0);
    let content_ctr =
        Point::new((page_bbox.ll().x + page_bbox.ur().x) / 2.0, (page_bbox.ll().y + page_bbox.ur().y) / 2.0);
    let mv = page_ctr - content_ctr;
    // The offset is snapped to KiCad's 1.27 mm grid so placement on the grid stays on it.
    const GRID_MM: f64 = 1.27;
    let mv = Point::new(pyround(mv.x / GRID_MM) * GRID_MM, pyround(mv.y / GRID_MM) * GRID_MM);
    (Tx::lin(MILS_TO_MM, 0.0, 0.0, -MILS_TO_MM).move_by(mv), paper)
}

/// True for KiCad's "#" references (#PWR, #FLG), which are never shown.
pub fn reference_is_hidden(ref_: &str) -> bool {
    ref_.starts_with('#')
}

/// Sheet box of one line of horizontal field text.
fn text_box(text: &str, spot: FieldSpot) -> Box4 {
    let (x, y, justify) = spot;
    let w = text.chars().count() as f64 * q::FONT_SIZE * q::CHAR_W;
    let h = q::FONT_SIZE * q::CHAR_H;
    let x1 = match justify {
        'l' => x,
        'r' => x - w,
        _ => x - w / 2.0,
    };
    (x1, y - h / 2.0, x1 + w, y + h / 2.0)
}

/// Convert a sheet-space field justification into KiCad's symbol-relative form.
///
/// KiCad stores a field's angle and justification relative to its symbol and
/// applies the symbol's rotation and mirroring when drawing it. Returns the
/// (angle, justification) to write.
fn kicad_field_orient(part_tx: Tx, sheet_tx: Tx, justify: char) -> (f64, Option<&'static str>) {
    let lin = (part_tx * sheet_tx).no_translate();
    // KiCad's symbol-local coordinates are the library coordinates with Y negated.
    let to_sheet = Tx::lin(1.0, 0.0, 0.0, -1.0) * lin;
    let options = [
        (Point::new(1.0, 0.0), 0.0, "left"),
        (Point::new(-1.0, 0.0), 0.0, "right"),
        (Point::new(0.0, -1.0), 90.0, "left"), // Angle-90 text reads toward local -Y.
        (Point::new(0.0, 1.0), 90.0, "right"),
    ];
    let mut found = options[3];
    for opt in options {
        let v = opt.0 * to_sheet;
        if v.x > 0.0 && v.y.abs() < 1e-6 * v.x.abs() {
            found = opt;
            break;
        }
    }
    let (_, angle, fwd) = found;
    match justify {
        'l' => (angle, Some(fwd)),
        'r' => (angle, Some(if fwd == "left" { "right" } else { "left" })),
        _ => (angle, None),
    }
}

/// The direction a pin points once its part's transformation is applied.
pub fn calc_pin_dir(pin: &Pin, part_tx: Tx) -> char {
    let tx = part_tx.no_translate();
    let v = match pin.orientation {
        'U' => Point::new(0.0, 1.0),
        'D' => Point::new(0.0, -1.0),
        'L' => Point::new(-1.0, 0.0),
        _ => Point::new(1.0, 0.0),
    } * tx;
    match (pyround(v.x) as i64, pyround(v.y) as i64) {
        (0, 1) => 'U',
        (0, -1) => 'D',
        (-1, 0) => 'L',
        _ => 'R',
    }
}

fn opposite_dir(d: char) -> char {
    match d {
        'U' => 'D',
        'D' => 'U',
        'L' => 'R',
        _ => 'L',
    }
}

/// Angle of a label or power symbol on a pin, so it extends away from the pin's part.
fn pin_label_angle(dir: char) -> f64 {
    match dir {
        'R' => 180.0,
        'L' => 0.0,
        'U' => 270.0,
        _ => 90.0,
    }
}

/// Where KiCad draws a pin, from its symbol's placement: (x, y) in sheet mm.
///
/// KiCad flips Y, rotates by the negative angle, then mirrors.
pub fn kicad_pin_pos(pin: &Pin, part_tx: Tx, sheet_tx: Tx) -> (f64, f64) {
    render_xy(pin.x, pin.y, part_tx, sheet_tx)
}

/// Where KiCad draws a point given in a symbol's coordinates (mm).
pub fn render_xy(lx: f64, ly: f64, part_tx: Tx, sheet_tx: Tx) -> (f64, f64) {
    let (angle, mx, my) = part_tx.analyze_transform();
    let composed = part_tx * sheet_tx;
    let (ox, oy) = (round_mm(composed.dx), round_mm(composed.dy));
    let (px, py) = (lx, -ly);
    let theta = (-angle).to_radians();
    let (c, s) = (theta.cos(), theta.sin());
    let (mut rx, mut ry) = (px * c - py * s, px * s + py * c);
    if mx {
        ry = -ry;
    }
    if my {
        rx = -rx;
    }
    (round_mm(ox + rx), round_mm(oy + ry))
}

fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let z = secs.div_euclid(86400) + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{:04}-{:02}-{:02}", y, m, d)
}

pub fn create_title_block_sexp(title: &str) -> Sexp {
    sx![
        "title_block",
        sx!["title", title],
        sx!["date", today()],
        sx!["company", ""],
        sx!["comment", 1, "Generated with SKiDL"],
        sx!["comment", 2, ""],
        sx!["comment", 3, ""],
        sx!["comment", 4, ""]
    ]
}

fn lib_id_of(part: &Part) -> String {
    let lib = part.lib.rsplit_once('.').map_or(part.lib.as_str(), |(stem, _)| stem);
    let lib = if lib.is_empty() { "Device" } else { lib };
    let name = if part.name.is_empty() { "Unknown" } else { &part.name };
    format!("{}:{}", lib, name)
}

/// The library symbol definition of a part, from its drawing commands.
pub fn part_to_lib_symbol_definition(part: &Part, tool: &str) -> Sexp {
    let part_name = if part.name.is_empty() { "Unknown" } else { &part.name };
    let kicad8 = tool == "kicad8";

    // KiCad 8 spells "hidden pin numbers" differently from KiCad 9 and 10.
    let pin_numbers = if kicad8 { sx!["pin_numbers", "hide"] } else { sx!["pin_numbers", sx!["hide", "yes"]] };
    let (offset, hide) = part.pin_names.unwrap_or((0.0, false));
    let mut pin_names = sx!["pin_names", sx!["offset", offset]];
    if hide {
        pin_names.push(if kicad8 { Sexp::from("hide") } else { sx!["hide", "yes"] });
    }

    let mut def = sx!["symbol", lib_id_of(part)];
    // A power symbol only connects its net by name if its definition says it is one.
    if part.is_power_symbol() {
        def.push(sx!["power"]);
    }
    def.push(pin_numbers);
    def.push(pin_names);
    def.push(sx!["exclude_from_sim", "no"]);
    def.push(sx!["in_bom", "yes"]);
    def.push(sx!["on_board", "yes"]);

    let font = || sx!["effects", sx!["font", sx!["size", 1.27, 1.27]]];
    let ref_prefix = if part.ref_prefix.is_empty() { "U" } else { &part.ref_prefix };
    let datasheet = if part.datasheet.is_empty() { "~" } else { &part.datasheet };
    def.push(sx!["property", "Reference", ref_prefix, sx!["at", 2.032, 0, 90], font()]);
    def.push(sx!["property", "Value", part_name, sx!["at", 0, 0, 90], font()]);
    def.push(hidden_property("Footprint", "", 0.0, 0.0, 0.0));
    def.push(hidden_property("Datasheet", datasheet, 0.0, 0.0, 0.0));
    if !part.description.is_empty() {
        def.push(hidden_property("Description", &part.description, 0.0, 0.0, 0.0));
    }

    // Graphics common to all units (unit 0).
    if let Some(cmds) = part.draw_cmds.get(&0) {
        let graphics: Vec<Sexp> = cmds.iter().filter(|c| !c.is("pin")).cloned().collect();
        if !graphics.is_empty() {
            let mut sub = sx!["symbol", format!("{}_0_1", part_name)];
            sub.list_mut().unwrap().extend(graphics);
            def.push(sub);
        }
    }
    // Graphics and pins of each unit. KiCad names unit sub-symbols
    // NAME_<unit>_<body style>; body style 1 is the normal one.
    for (unit_num, cmds) in part.draw_cmds.iter() {
        if *unit_num == 0 {
            continue;
        }
        let pins: Vec<Sexp> = cmds.iter().filter(|c| c.is("pin")).cloned().collect();
        let graphics: Vec<Sexp> = cmds.iter().filter(|c| !c.is("pin") && !c.is("property")).cloned().collect();
        if !pins.is_empty() || !graphics.is_empty() {
            let mut sub = sx!["symbol", format!("{}_{}_1", part_name, unit_num)];
            sub.list_mut().unwrap().extend(graphics);
            sub.list_mut().unwrap().extend(pins);
            def.push(sub);
        }
    }

    // KiCad 9 and 10 carry an embedded-fonts flag on each lib symbol; KiCad 8 omits it.
    if !kicad8 {
        def.push(sx!["embedded_fonts", "no"]);
    }
    def
}

/// The wire segments of a net, split at its junctions.
///
/// A junction in the middle of a wire doesn't connect a pin there unless the
/// wire is split into separate segments at it.
pub fn wire_to_sexp(net_name: &str, wire: &[Segment], tx: Tx, junctions: &[Point]) -> Vec<Sexp> {
    let juncs: Vec<(f64, f64)> = junctions
        .iter()
        .map(|j| {
            let jt = *j * tx;
            (round_mm(jt.x), round_mm(jt.y))
        })
        .collect();
    let make =
        |x1: f64, y1: f64, x2: f64, y2: f64| wire_sexp(x1, y1, x2, y2, &format!("wire:{}:{}:{}:{}:{}", net_name, f(x1), f(y1), f(x2), f(y2)));

    let mut wires = Vec::new();
    for segment in wire {
        let w = *segment * tx;
        let (x1, y1) = (round_mm(w.p1.x), round_mm(w.p1.y));
        let (x2, y2) = (round_mm(w.p2.x), round_mm(w.p2.y));

        // The junctions strictly between the ends.
        let mut pts: Vec<f64> = Vec::new();
        for &(jx, jy) in &juncs {
            if x1 == x2 && x1 == jx {
                if y1.min(y2) < jy && jy < y1.max(y2) {
                    pts.push(jy);
                }
            } else if y1 == y2 && y1 == jy && x1.min(x2) < jx && jx < x1.max(x2) {
                pts.push(jx);
            }
        }
        if pts.is_empty() {
            wires.push(make(x1, y1, x2, y2));
            continue;
        }
        let vertical = x1 == x2;
        let (a, b) = if vertical { (y1, y2) } else { (x1, x2) };
        pts.push(a);
        pts.push(b);
        pts.sort_by(|p, q| p.total_cmp(q));
        pts.dedup();
        if a > b {
            pts.reverse();
        }
        for pair in pts.windows(2) {
            if vertical {
                wires.push(make(x1, pair[0], x2, pair[1]));
            } else {
                wires.push(make(pair[0], y1, pair[1], y2));
            }
        }
    }
    wires
}

fn mm_key(x: f64, y: f64) -> (i64, i64) {
    ((x * 100.0).round() as i64, (y * 100.0).round() as i64)
}

fn wire_endpoints(el: &Sexp) -> Vec<(i64, i64)> {
    el.field("pts").map(|p| p.fields("xy").map(|xy| mm_key(xy.num(1), xy.num(2))).collect()).unwrap_or_default()
}

/// Names of the `power:*` symbols instanced in some elements.
fn power_lib_ids_in_elements(elements: &[Sexp]) -> Vec<String> {
    let mut found: Vec<String> = elements
        .iter()
        .filter(|e| e.is("symbol"))
        .filter_map(|e| e.field("lib_id"))
        .map(|l| l.str_at(1).to_string())
        .filter(|l| l.starts_with("power:"))
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Write a schematic to a file, quoting the strings KiCad wants quoted.
pub fn write_sexp_schematic(mut schematic: Sexp, filepath: &std::path::Path) -> Res<()> {
    schematic.add_kicad_quotes();
    std::fs::write(filepath, schematic.to_text())?;
    Ok(())
}

/// What field text has to stay off on a sheet.
struct Obstacles {
    /// Boxes of text and power symbols.
    boxes: Vec<Box4>,
    wires: Vec<Box4>,
    /// Outlines of the power symbols.
    symbols: Vec<Box4>,
    /// Visible text that isn't a part's to move, each with the index of its own symbol's outline.
    texts: Vec<(Box4, Option<usize>)>,
}

fn sheet_obstacles(elements: &[Sexp]) -> Obstacles {
    let mut all_texts: Vec<Box4> = Vec::new();
    let mut texts: Vec<(Box4, Option<usize>)> = Vec::new();
    for label in elements.iter().filter(|e| e.is("global_label")) {
        let b = q::label_box(label);
        all_texts.push(b);
        texts.push((b, None));
    }
    let mut symbols: Vec<Box4> = Vec::new();
    // Only power symbols are among the elements.
    for symbol in elements.iter().filter(|e| e.is("symbol")) {
        let to_sheet = InstanceMap::new(symbol);
        let lib_id = symbol.field("lib_id").map_or("", |l| l.str_at(1));
        let name = lib_id.split_once(':').map_or("", |(_, n)| n);
        let pts: Vec<(f64, f64)> = power_lib()
            .get(name)
            .map(|sym| q::symbol_points(sym, 1).iter().map(|&(x, y)| to_sheet.map(x, y)).collect())
            .unwrap_or_default();
        let mut outline = None;
        if !pts.is_empty() {
            symbols.push(q::bounds(&pts));
            outline = Some(symbols.len() - 1);
        }
        for field in symbol.fields("property") {
            let fname = field.str_at(1);
            if fname == "Reference" || fname == "Value" {
                let b = q::field_box(symbol, field, &to_sheet);
                // A hidden field (a power symbol's reference) is still kept clear of.
                all_texts.push(b);
                if field.get(2).is_some_and(|t| !t.text().is_empty()) && !q::is_hidden(field) {
                    texts.push((b, outline));
                }
            }
        }
    }
    let mut boxes = all_texts;
    boxes.extend(symbols.iter().copied());
    let mut wires = Vec::new();
    for wire in elements.iter().filter(|e| e.is("wire")) {
        if let Some(pts) = wire.field("pts") {
            let xy: Vec<&Sexp> = pts.fields("xy").collect();
            if xy.len() >= 2 {
                let (x1, y1, x2, y2) = (xy[0].num(1), xy[0].num(2), xy[1].num(1), xy[1].num(2));
                wires.push((x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2)));
            }
        }
    }
    Obstacles { boxes, wires, symbols, texts }
}

impl Design {
    /// A part's pin points and body box in sheet mm.
    fn field_body(&self, part: PartId, sheet_tx: Tx) -> (Vec<Point>, BBox) {
        let p = &self.c.parts[part];
        let tx = p.tx * sheet_tx;
        match p.body_bbox {
            Some(body) => (p.pins.iter().map(|&pin| self.c.pins[pin].pt * tx).collect(), body * tx),
            None => {
                // No symbol box (labels-only mode), so allow a little room around the origin.
                let org = tx.origin();
                (Vec::new(), BBox::new(org, org).resize(Point::new(1.27, 1.27)))
            }
        }
    }

    /// Where a part's Reference and Value go if nothing else is in the way:
    /// horizontal, on sides of the body that have no pins.
    fn default_field_layout(&self, part: PartId, sheet_tx: Tx) -> FieldLayout {
        let (pin_pts, body) = self.field_body(part, sheet_tx);

        // Pins sit on the edges of the body box.
        let eps = 0.1;
        let (mut l, mut r, mut t, mut b) = (false, false, false, false);
        for pt in &pin_pts {
            l |= pt.x <= body.min.x + eps;
            r |= pt.x >= body.max.x - eps;
            t |= pt.y <= body.min.y + eps;
            b |= pt.y >= body.max.y - eps;
        }
        let ctr = body.ctr();
        let above = body.min.y - FIELD_GAP_MM - FIELD_PITCH_MM / 2.0;
        let below = body.max.y + FIELD_GAP_MM + FIELD_PITCH_MM / 2.0;
        let right = body.max.x + FIELD_GAP_MM;
        let left = body.min.x - FIELD_GAP_MM;
        let half = FIELD_PITCH_MM / 2.0;

        if !t && !b {
            // Pins on the left and right: reference above, value below.
            return ((ctr.x, above, 'c'), (ctr.x, below, 'c'));
        }
        if !r {
            return ((right, ctr.y - half, 'l'), (right, ctr.y + half, 'l'));
        }
        if !l {
            return ((left, ctr.y - half, 'r'), (left, ctr.y + half, 'r'));
        }
        if !t {
            return ((ctr.x, above - FIELD_PITCH_MM, 'c'), (ctr.x, above, 'c'));
        }
        if !b {
            return ((ctr.x, below, 'c'), (ctr.x, below + FIELD_PITCH_MM, 'c'));
        }
        // Pins on every side: tuck the fields just outside the left corners.
        ((body.min.x, above, 'l'), (body.min.x, below, 'l'))
    }

    /// Where a part's Reference and Value go: the planned spot, if there is one.
    pub fn calc_field_layout(&self, part: PartId, sheet_tx: Tx) -> FieldLayout {
        self.c.parts[part].field_layout.unwrap_or_else(|| self.default_field_layout(part, sheet_tx))
    }

    /// The places a part's Reference and Value could go, best first.
    fn field_candidates(&self, part: PartId, sheet_tx: Tx) -> Vec<FieldLayout> {
        let mut candidates = vec![self.default_field_layout(part, sheet_tx)];
        let body = self.field_body(part, sheet_tx).1;
        let ctr = body.ctr();
        let above = body.min.y - FIELD_GAP_MM - FIELD_PITCH_MM / 2.0;
        let below = body.max.y + FIELD_GAP_MM + FIELD_PITCH_MM / 2.0;
        let (half, pitch) = (FIELD_PITCH_MM / 2.0, FIELD_PITCH_MM);
        candidates.push(((ctr.x, above, 'c'), (ctr.x, below, 'c')));
        candidates.push(((ctr.x, above - pitch, 'c'), (ctr.x, above, 'c')));
        candidates.push(((ctr.x, below, 'c'), (ctr.x, below + pitch, 'c')));
        for out in [FIELD_GAP_MM, FIELD_GAP_MM + 2.0 * pitch] {
            let (right, left) = (body.max.x + out, body.min.x - out);
            candidates.push(((right, ctr.y - half, 'l'), (right, ctr.y + half, 'l')));
            candidates.push(((left, ctr.y - half, 'r'), (left, ctr.y + half, 'r')));
            candidates.push(((right, above - pitch, 'l'), (right, above, 'l')));
            candidates.push(((left, above - pitch, 'r'), (left, above, 'r')));
            candidates.push(((right, below, 'l'), (right, below + pitch, 'l')));
            candidates.push(((left, below, 'r'), (left, below + pitch, 'r')));
        }
        candidates
    }

    /// The Reference and Value text a part shows; "" for one it doesn't.
    fn field_texts(&self, part: PartId) -> [String; 2] {
        let p = &self.c.parts[part];
        let ref_ = if reference_is_hidden(&p.base_ref) { String::new() } else { p.base_ref.clone() };
        [ref_, p.value.clone()]
    }

    /// Move each part's Reference and Value to the spot with the fewest collisions.
    ///
    /// Returns how many pairs of symbols overlap on the sheet and how many
    /// collisions its text is left with.
    fn choose_field_layouts(&mut self, placed: &[(PartId, Tx)], elements: &[Sexp], sheet_boxes: &[Box4]) -> (usize, usize) {
        let obstacles = sheet_obstacles(elements);
        let mut fixed = obstacles.boxes.clone();
        fixed.extend(sheet_boxes.iter().copied());
        let wires = &obstacles.wires;

        let bodies: Vec<Box4> = placed.iter().map(|&(part, tx)| self.field_body(part, tx).1.tuple()).collect();
        let texts: Vec<[String; 2]> = placed.iter().map(|&(part, _)| self.field_texts(part)).collect();

        let text_boxes = |i: usize, layout: &FieldLayout| -> Vec<Box4> {
            let mut boxes = Vec::new();
            if !texts[i][0].is_empty() {
                boxes.push(text_box(&texts[i][0], layout.0));
            }
            if !texts[i][1].is_empty() {
                boxes.push(text_box(&texts[i][1], layout.1));
            }
            boxes
        };

        let mut chosen: Vec<Vec<Box4>> =
            placed.iter().enumerate().map(|(i, &(part, tx))| text_boxes(i, &self.calc_field_layout(part, tx))).collect();

        fn collisions(i: usize, boxes: &[Box4], chosen: &[Vec<Box4>], fixed: &[Box4], bodies: &[Box4], wires: &[Box4]) -> usize {
            let mut n = 0;
            for b in boxes {
                n += fixed.iter().filter(|o| q::overlap(b, o)).count();
                for (j, group) in chosen.iter().enumerate() {
                    if j != i {
                        n += group.iter().filter(|o| q::overlap(b, o)).count();
                    }
                }
                n += bodies.iter().filter(|o| q::overlap(b, o)).count();
                n += wires.iter().filter(|w| q::crosses(b, w)).count();
            }
            n
        }

        // A part's move can free up a better spot for one placed earlier.
        for _ in 0..2 {
            for (i, &(part, tx)) in placed.iter().enumerate() {
                let candidates = self.field_candidates(part, tx);
                let mut best_cost = usize::MAX;
                let mut best = candidates[0];
                for layout in &candidates {
                    let cost = collisions(i, &text_boxes(i, layout), &chosen, &fixed, &bodies, wires);
                    if cost < best_cost {
                        best_cost = cost;
                        best = *layout;
                    }
                    if cost == 0 {
                        break;
                    }
                }
                if best_cost > 0 {
                    // The candidates keep Reference and Value together. Where a
                    // neighbor leaves no room for both, one of them can often
                    // still fit by itself.
                    let mut layout = [best.0, best.1];
                    let mut spots: Vec<FieldSpot> = Vec::new();
                    for pair in &candidates {
                        for spot in [pair.0, pair.1] {
                            if !spots.contains(&spot) {
                                spots.push(spot);
                            }
                        }
                    }
                    for k in 0..2 {
                        let text = &texts[i][k];
                        if text.is_empty() {
                            continue;
                        }
                        let mut cost = collisions(i, &[text_box(text, layout[k])], &chosen, &fixed, &bodies, wires);
                        if cost == 0 {
                            continue;
                        }
                        let other = 1 - k;
                        let has_other = !texts[i][other].is_empty();
                        let sibling = has_other.then(|| text_box(&texts[i][other], layout[other]));
                        for &spot in &spots {
                            let b = text_box(text, spot);
                            if sibling.is_some_and(|s| q::overlap(&b, &s)) {
                                continue;
                            }
                            if has_other && (spot.0, spot.2) == (layout[other].0, layout[other].2) {
                                // In one column, the reference stays above the value.
                                let (ref_y, val_y) = if k == 0 { (spot.1, layout[1].1) } else { (layout[0].1, spot.1) };
                                if ref_y > val_y {
                                    continue;
                                }
                            }
                            let spot_cost = collisions(i, &[b], &chosen, &fixed, &bodies, wires);
                            if spot_cost < cost {
                                cost = spot_cost;
                                layout[k] = spot;
                            }
                            if cost == 0 {
                                break;
                            }
                        }
                    }
                    best = (layout[0], layout[1]);
                }
                self.c.parts[part].field_layout = Some(best);
                chosen[i] = text_boxes(i, &best);
            }
        }

        let mut symbols = bodies.clone();
        let n_bodies = symbols.len();
        symbols.extend(obstacles.symbols.iter().copied());
        let mut part_overlaps = 0;
        for (i, b) in symbols.iter().enumerate() {
            part_overlaps += symbols[i + 1..].iter().filter(|o| q::overlap(b, o)).count();
        }
        // Text that isn't a part's to move: labels and the names of power symbols.
        let mut text_overlaps = 0;
        for (i, (b, _)) in obstacles.texts.iter().enumerate() {
            text_overlaps += obstacles.texts[i + 1..].iter().filter(|(o, _)| q::overlap(b, o)).count();
        }
        for (b, own) in &obstacles.texts {
            text_overlaps += symbols
                .iter()
                .enumerate()
                .filter(|(j, s)| own.map(|o| o + n_bodies) != Some(*j) && q::overlap(b, s))
                .count();
        }
        for (i, boxes) in chosen.iter().enumerate() {
            text_overlaps += collisions(i, boxes, &chosen, &fixed, &bodies, wires);
        }
        (part_overlaps, text_overlaps)
    }

    /// Collect what's drawn on a node's sheet: by the node and the flattened nodes below it.
    ///
    /// Returns the sheet's connectivity elements, its (part, sheet transform)
    /// pairs, and the boxes of the child sheets drawn on it.
    fn gather_sheet(&mut self, node: NodeId, tx: Tx, plan_below: bool) -> (Vec<Sexp>, Vec<(PartId, Tx)>, Vec<Box4>) {
        let (mut elements, mut placed, mut sheet_boxes) = (Vec::new(), Vec::new(), Vec::new());
        for child in self.children(node) {
            if self.nodes[child].flattened {
                let child_tx = self.nodes[child].tx * tx;
                let (e, p, b) = self.gather_sheet(child, child_tx, plan_below);
                elements.extend(e);
                placed.extend(p);
                sheet_boxes.extend(b);
            } else {
                if plan_below {
                    self.plan_field_layouts(child, true);
                }
                let bbox = self.nodes[child].bbox * self.nodes[child].tx * tx;
                sheet_boxes.push(bbox.tuple());
            }
        }
        for &part in &self.nodes[node].parts {
            if !self.c.parts[part].is_net_terminal {
                placed.push((part, tx));
            }
        }
        for &(part, _) in &placed {
            self.c.parts[part].field_layout = None;
        }
        let elements = self.sheet_connectivity(node, elements, tx, None);
        (elements, placed, sheet_boxes)
    }

    /// Choose where each part's Reference and Value go on a node's sheet.
    ///
    /// The sheet is gathered the way the writers do it, so the text can avoid
    /// the labels and wires they'll draw. Returns (part overlaps, text overlaps).
    pub fn plan_field_layouts(&mut self, node: NodeId, plan_below: bool) -> (usize, usize) {
        let tx = calc_sheet_tx(&self.internal_bbox(node)).0;
        let (mut elements, placed, sheet_boxes) = self.gather_sheet(node, tx, plan_below);
        self.deconflict_sheet_labels(&mut elements, node, tx);
        self.choose_field_layouts(&placed, &elements, &sheet_boxes)
    }

    /// Count the overlaps a placed and routed sheet would be drawn with,
    /// without writing it: (part overlaps, text overlaps).
    pub fn sheet_overlaps(&mut self, node: NodeId) -> (usize, usize) {
        self.plan_field_layouts(node, false)
    }

    /// The S-expression of a symbol instance.
    pub fn part_to_sexp(&self, part: PartId, uuid_path: &str, sheet_tx: Tx) -> Sexp {
        let p = &self.c.parts[part];
        let (angle, mx, my) = p.tx.analyze_transform();
        let tx = p.tx * sheet_tx;
        let origin = Point::new(round_mm(tx.dx), round_mm(tx.dy));
        let unit_num = p.num;
        // All the units of a part share its reference and are told apart by (unit N).
        let base_ref = p.base_ref.as_str();
        let hiername = p.hiername();

        let mut symbol = sx!["symbol", sx!["lib_id", lib_id_of(p)], sx!["at", origin.x, origin.y, angle]];
        if mx {
            symbol.push(sx!["mirror", "x"]);
        } else if my {
            symbol.push(sx!["mirror", "y"]);
        }
        symbol.push(sx!["unit", unit_num]);
        symbol.push(sx!["exclude_from_sim", "no"]);
        symbol.push(sx!["in_bom", "yes"]);
        symbol.push(sx!["on_board", "yes"]);
        symbol.push(sx!["dnp", "no"]);
        symbol.push(sx!["fields_autoplaced", "yes"]);
        symbol.push(sx!["uuid", gen_uuid(&hiername)]);

        // Reference and Value: horizontal text on pin-free sides of the body,
        // given relative to the symbol's orientation the way KiCad stores it.
        let (ref_spot, val_spot) = self.calc_field_layout(part, sheet_tx);
        let (ref_angle, ref_just) = kicad_field_orient(p.tx, sheet_tx, ref_spot.2);
        let (val_angle, val_just) = kicad_field_orient(p.tx, sheet_tx, val_spot.2);
        symbol.push(sx![
            "property",
            "Reference",
            base_ref,
            sx!["at", round_mm(ref_spot.0), round_mm(ref_spot.1), ref_angle],
            field_effects(ref_just, reference_is_hidden(base_ref))
        ]);
        symbol.push(sx![
            "property",
            "Value",
            p.value.as_str(),
            sx!["at", round_mm(val_spot.0), round_mm(val_spot.1), val_angle],
            field_effects(val_just, false)
        ]);
        symbol.push(hidden_property("Footprint", &p.footprint, origin.x, origin.y, angle));
        let datasheet = if p.datasheet.is_empty() { "~" } else { &p.datasheet };
        symbol.push(hidden_property("Datasheet", datasheet, origin.x, origin.y, angle));
        symbol.push(hidden_property("Description", &p.description, origin.x, origin.y, angle));

        // Fields the user added to the part.
        let mut y_offset = 5.08;
        for (name, value) in &p.fields {
            if matches!(name.to_lowercase().as_str(), "reference" | "value" | "footprint" | "datasheet" | "description") {
                continue;
            }
            if !value.trim().is_empty() {
                symbol.push(hidden_property(name, value, origin.x, origin.y + y_offset, angle));
                y_offset += 1.27;
            }
        }

        // KiCad needs the pins listed to track connectivity.
        for &pin in &p.pins {
            let num = &self.c.pins[pin].num;
            symbol.push(sx!["pin", sexp::q(num), sx!["uuid", gen_uuid(&format!("{}_pin_{}", hiername, num))]]);
        }

        symbol.push(sx![
            "instances",
            sx![
                "project",
                self.kc.project_name.as_str(),
                sx!["path", uuid_path, sx!["reference", base_ref], sx!["unit", unit_num]]
            ]
        ]);
        symbol
    }

    /// The direction used to orient a pin's label or power symbol.
    ///
    /// A net terminal's "body" is the label itself, with its routing coming in
    /// from the other side, so its direction is reversed.
    pub fn calc_label_dir(&self, pin: PinId) -> char {
        let part = self.c.pin_part(pin);
        let dir = calc_pin_dir(&self.c.pins[pin], part.tx);
        if part.is_net_terminal {
            opposite_dir(dir)
        } else {
            dir
        }
    }

    fn power_symbol_to_sexp(&mut self, pin: PinId, net_name: &str, tx: Tx) -> Sexp {
        self.kc.pwr_counter += 1;
        let pwr_ref = format!("#PWR{:03}", self.kc.pwr_counter);
        let pt = self.c.pins[pin].pt * (self.c.pin_part(pin).tx * tx);
        let (x, y) = (round_mm(pt.x), round_mm(pt.y));

        // The symbol's body is lined up with the direction the pin's stub leaves it.
        let intrinsic = power_symbol_pin_angle(net_name);
        let angle = (pin_label_angle(self.calc_label_dir(pin)) - intrinsic).rem_euclid(360.0);
        let uuid_path = if self.kc.sheet_uuid_path.is_empty() {
            format!("/{}", gen_uuid("root_schematic"))
        } else {
            self.kc.sheet_uuid_path.clone()
        };
        power_symbol_instance_sexp(
            &self.kc.project_name,
            net_name,
            x,
            y,
            angle,
            &pwr_ref,
            &uuid_path,
            &format!("{}:{}:{}:{}", net_name, f(x), f(y), self.kc.pwr_counter),
        )
    }

    /// The label at a pin's stub: a power symbol if the net is named like one, else a global label.
    ///
    /// With `force`, the pin gets a label whether it's stubbed or not (net
    /// terminals always need one).
    pub fn net_label_to_sexp(&mut self, pin: PinId, tx: Tx, force: bool) -> Option<Sexp> {
        if !force && (!self.c.pins[pin].stub || !self.c.is_connected(pin)) {
            return None;
        }
        let net = self.c.pins[pin].net?;
        if self.c.nets[net].nc {
            return None;
        }
        // A power symbol already labels its net.
        if self.c.pin_part(pin).is_power_symbol() {
            return None;
        }
        let name = self.c.nets[net].name.clone();
        if is_power_symbol_name(&name) {
            return Some(self.power_symbol_to_sexp(pin, &name, tx));
        }

        // The label's text extends away from the pin, clear of the part's body.
        let pt = self.c.pins[pin].pt * self.c.pin_part(pin).tx * tx;
        let angle = pin_label_angle(self.calc_label_dir(pin));
        let justify = if angle == 0.0 || angle == 90.0 { "left" } else { "right" };
        Some(sx![
            "global_label",
            name.as_str(),
            sx!["shape", "bidirectional"],
            sx!["at", round_mm(pt.x), round_mm(pt.y), angle],
            sx!["effects", sx!["font", sx!["size", 1.27, 1.27]], sx!["justify", justify]],
            sx!["uuid", gen_uuid(&format!("label:{}:{}:{}", name, pt.x, pt.y))]
        ])
    }

    /// The symbol of a hierarchical sheet for its parent's sheet, then the labels on its pins.
    fn create_hierarchical_sheet_sexp(&self, node: NodeId, sheet_uuid: &str, sheet_tx: Tx) -> Vec<Sexp> {
        let n = &self.nodes[node];
        let bbox = self.sheet_rect(node) * n.tx * sheet_tx;
        let (bx, by) = (round_mm(bbox.ll().x), round_mm(bbox.ll().y));
        let (bw, bh) = (round_mm(bbox.w()), round_mm(bbox.h()));
        let font = || sx!["font", sx!["size", 1.27, 1.27]];

        let mut sheet = sx![
            "sheet",
            sx!["at", bx, by],
            sx!["size", bw, bh],
            sx!["exclude_from_sim", "no"],
            sx!["in_bom", "yes"],
            sx!["on_board", "yes"],
            sx!["dnp", "no"],
            sx!["fields_autoplaced", "yes"],
            sx!["stroke", sx!["width", 0.1524], sx!["type", "solid"]],
            sx!["fill", sx!["color", 0, 0, 0, 0]],
            sx!["uuid", sheet_uuid],
            sx![
                "property",
                "Sheetname",
                n.name.as_str(),
                sx!["at", bx, round_mm(by - 0.7116), 0],
                sx!["effects", font(), sx!["justify", "left", "bottom"]]
            ],
            sx![
                "property",
                "Sheetfile",
                n.sheet_filename.as_str(),
                sx!["at", bx, round_mm(by + bh + 0.5846), 0],
                sx!["effects", font(), sx!["justify", "left", "top"]]
            ]
        ];

        // One pin for each hierarchical label in the child, down the sheet's
        // left edge. A pin with no label inside, or a label with no pin, is an
        // ERC error, so the pins come from the labels the child's sheet got.
        let mut labels = Vec::new();
        let pin_spacing = 2.54;
        let mut pin_y = by + pin_spacing;
        for name in &n.hier_label_names {
            sheet.push(sx![
                "pin",
                sexp::q(name),
                "bidirectional",
                sx!["at", bx, round_mm(pin_y), 180],
                sx!["effects", font(), sx!["justify", "left"]],
                sx!["uuid", gen_uuid(&format!("sheet_pin:{}:{}", n.sheet_filename, name))]
            ]);
            labels.push(sx![
                "global_label",
                name.as_str(),
                sx!["shape", "bidirectional"],
                sx!["at", bx, round_mm(pin_y), 180],
                sx!["effects", font(), sx!["justify", "right"]],
                sx!["uuid", gen_uuid(&format!("sheet_pin_label:{}:{}", n.sheet_filename, name))]
            ]);
            pin_y += pin_spacing;
        }
        let mut out = vec![sheet];
        out.extend(labels);
        out
    }

    /// Names of the nets a child sheet passes to its parent through hierarchical labels.
    ///
    /// These are its boundary nets, less the ones that connect another way:
    /// power nets (power symbols) and stubbed nets (global labels).
    fn hier_label_nets(&self, node: NodeId) -> HashSet<String> {
        if self.nodes[node].parent.is_none() {
            return HashSet::new();
        }
        self.get_boundary_nets(node)
            .into_iter()
            .filter(|&n| !is_power_symbol_name(&self.c.nets[n].name) && !self.c.nets[n].stub)
            .map(|n| self.c.nets[n].name.clone())
            .collect()
    }

    /// Turn a sheet's global labels for its boundary nets into hierarchical labels.
    fn use_hierarchical_labels(&mut self, node: NodeId, elements: &mut [Sexp]) {
        let hier_nets = self.hier_label_nets(node);
        let mut names: Vec<String> = Vec::new();
        for elem in elements.iter_mut() {
            if elem.is("global_label") && hier_nets.contains(elem.str_at(1)) {
                let name = elem.str_at(1).to_string();
                elem.list_mut().unwrap()[0] = Sexp::from("hierarchical_label");
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        self.nodes[node].hier_label_names = names;
    }

    /// Add a node's labels, wires and junctions to its sheet's elements.
    ///
    /// Every output format draws this list, so what's decided here (which
    /// wires are kept, where labels go) comes out the same in all of them.
    /// With `emit_parts`, the elements of the node's parts are added too, for
    /// the sheet with that instance path.
    pub fn sheet_connectivity(&mut self, node: NodeId, mut elements: Vec<Sexp>, tx: Tx, emit_parts: Option<&str>) -> Vec<Sexp> {
        let parts = self.nodes[node].parts.clone();

        // Nets with stubbed pins of real parts on this sheet. Those pins get
        // labels of their own, so a net terminal's label would be redundant.
        let mut nets_with_real_pins: HashSet<NetId> = HashSet::new();
        for &part in &parts {
            if self.c.parts[part].is_net_terminal {
                continue;
            }
            for &pin in &self.c.parts[part].pins {
                if self.c.pins[pin].stub && self.c.is_connected(pin) {
                    nets_with_real_pins.insert(self.c.pins[pin].net.unwrap());
                }
            }
        }

        // A routed net with exactly one pin of a real part on this sheet gets
        // its label at that pin instead of at its net terminal, and the wire
        // between the two is left out.
        let mut onpin_nets: HashSet<NetId> = HashSet::new();
        for &part in &parts {
            if self.c.parts[part].is_net_terminal {
                let pin = self.c.parts[part].pins[0];
                if !self.c.is_connected(pin) {
                    continue;
                }
                let net = self.c.pins[pin].net.unwrap();
                if nets_with_real_pins.contains(&net) {
                    continue;
                }
                let real_pins: Vec<PinId> = self.c.nets[net]
                    .pins
                    .iter()
                    .copied()
                    .filter(|&p| self.pin_in_node(p, node) && !self.c.pin_part(p).is_net_terminal)
                    .collect();
                if real_pins.len() == 1 && self.c.is_connected(real_pins[0]) && !self.c.pins[real_pins[0]].stub {
                    if let Some(label) = self.net_label_to_sexp(real_pins[0], tx, true) {
                        elements.push(label);
                        onpin_nets.insert(net);
                    }
                    continue;
                }
                if let Some(label) = self.net_label_to_sexp(pin, tx, true) {
                    elements.push(label);
                }
            } else if let Some(uuid_path) = emit_parts {
                elements.push(self.part_to_sexp(part, uuid_path, tx));
            }
        }

        // Wires, split at junctions, then the junctions. Nets that were turned
        // into stubs after routing are skipped: their wires are stale.
        for (&net, wire) in &self.nodes[node].wires {
            if self.c.nets[net].stub || onpin_nets.contains(&net) {
                continue;
            }
            let empty = Vec::new();
            let juncs = self.nodes[node].junctions.get(&net).unwrap_or(&empty);
            elements.extend(wire_to_sexp(&self.c.nets[net].name, wire, tx, juncs));
        }
        for (&net, junctions) in &self.nodes[node].junctions {
            if self.c.nets[net].stub || onpin_nets.contains(&net) {
                continue;
            }
            for j in junctions {
                let pt = *j * tx;
                let (x, y) = (round_mm(pt.x), round_mm(pt.y));
                elements.push(junction_sexp(x, y, &format!("junction:{}:{}", f(x), f(y))));
            }
        }

        // Labels of pins that snapping put on top of each other are redundant.
        let mut wired_pins = self.find_overlapping_pins(node, tx);

        // Power pins in a line are joined by a bus wire.
        let (bus_segments, bus_pins) = self.find_power_bus_runs(node, tx);
        for (x1, y1, x2, y2, name) in bus_segments {
            elements.push(wire_sexp(x1, y1, x2, y2, &format!("pbus:{}:{}:{}:{}:{}", name, f(x1), f(y1), f(x2), f(y2))));
        }
        wired_pins.extend(bus_pins);

        // Wires and junctions that snapping added.
        let n = &self.nodes[node];
        let to_mm = |w: &(f64, f64, f64, f64)| {
            let p1 = Point::new(w.0, w.1) * tx;
            let p2 = Point::new(w.2, w.3) * tx;
            (round_mm(p1.x), round_mm(p1.y), round_mm(p2.x), round_mm(p2.y))
        };
        for w in &n.tjunction_wires {
            let (x1, y1, x2, y2) = to_mm(w);
            elements.push(wire_sexp(x1, y1, x2, y2, &format!("tjwire:{}:{}:{}:{}", f(x1), f(y1), f(x2), f(y2))));
        }
        for j in &n.tjunction_junctions {
            let jp = *j * tx;
            let (jx, jy) = (round_mm(jp.x), round_mm(jp.y));
            elements.push(junction_sexp(jx, jy, &format!("tjjunction:{}:{}", f(jx), f(jy))));
        }
        wired_pins.extend(n.tjunction_suppressed_pins.iter().copied());
        for w in &n.power_cap_wires {
            let (x1, y1, x2, y2) = to_mm(w);
            elements.push(wire_sexp(x1, y1, x2, y2, &format!("pcwire:{}:{}:{}:{}", f(x1), f(y1), f(x2), f(y2))));
        }
        for j in &n.power_cap_junctions {
            let jp = *j * tx;
            let (jx, jy) = (round_mm(jp.x), round_mm(jp.y));
            elements.push(junction_sexp(jx, jy, &format!("pcjunction:{}:{}", f(jx), f(jy))));
        }
        wired_pins.extend(n.power_cap_suppressed_pins.iter().copied());

        // Labels for the stubbed pins, except those that got wires instead.
        for &part in &parts {
            if self.c.parts[part].is_net_terminal {
                continue;
            }
            for pin in self.c.parts[part].pins.clone() {
                if wired_pins.contains(&pin) {
                    continue;
                }
                if let Some(label) = self.net_label_to_sexp(pin, tx, false) {
                    elements.push(label);
                }
            }
        }

        // No-connect flags.
        for (x, y, part_ref, pin_num) in self.find_no_connect_pins(node, tx) {
            elements.push(sx![
                "no_connect",
                sx!["at", round_mm(x), round_mm(y)],
                sx!["uuid", gen_uuid(&format!("nc:{}:{}:{}:{}", part_ref, pin_num, x, y))]
            ]);
        }

        // Remove wires left dangling. Snapping moves parts, which can leave a
        // routed wire ending on nothing: not a pin, label, junction,
        // no-connect flag or another wire's end. Removing one can expose the next.
        let mut anchors: HashSet<(i64, i64)> = HashSet::new();
        for &part in &parts {
            let p = &self.c.parts[part];
            for &pin in &p.pins {
                let w = self.c.pins[pin].pt * p.tx * tx;
                anchors.insert(mm_key(round_mm(w.x), round_mm(w.y)));
            }
        }
        // Flattened children's wires are among the elements too, and end on their pins.
        for child in self.children(node) {
            if self.nodes[child].flattened {
                anchors.extend(self.nodes[child].pin_anchor_pts.iter().copied());
            }
        }
        self.nodes[node].pin_anchor_pts = anchors.clone();
        for el in &elements {
            if matches!(el.head(), Some("global_label" | "label" | "junction" | "no_connect")) {
                if let Some(at) = el.field("at") {
                    anchors.insert(mm_key(at.num(1), at.num(2)));
                }
            }
        }
        for _ in 0..8 {
            let mut counts: HashMap<(i64, i64), usize> = HashMap::new();
            let mut wires: Vec<(usize, Vec<(i64, i64)>)> = Vec::new();
            for (i, el) in elements.iter().enumerate() {
                if el.is("wire") {
                    let pts = wire_endpoints(el);
                    for p in &pts {
                        *counts.entry(*p).or_insert(0) += 1;
                    }
                    wires.push((i, pts));
                }
            }
            let drop: HashSet<usize> = wires
                .iter()
                .filter(|(_, pts)| pts.iter().any(|p| !anchors.contains(p) && counts.get(p).copied().unwrap_or(0) < 2))
                .map(|(i, _)| *i)
                .collect();
            if drop.is_empty() {
                break;
            }
            let mut i = 0;
            elements.retain(|_| {
                i += 1;
                !drop.contains(&(i - 1))
            });
        }
        elements
    }

    /// Move net labels off part bodies, adding a short wire to each one moved.
    fn apply_label_deconfliction(&self, elements: &mut Vec<Sexp>, node: NodeId, sheet_tx: Tx) {
        const GRID: f64 = 1.27;
        let cell = |x: f64, y: f64| (pyround(x / GRID) as i64, pyround(y / GRID) as i64);

        // Label anchors (by net) and wire ends (no net) already on the grid, in element order.
        let mut occupied_seed: Vec<((i64, i64), Option<String>)> = Vec::new();
        let mut labels: Vec<(usize, String, f64, f64, f64)> = Vec::new();
        for (i, elem) in elements.iter().enumerate() {
            if elem.is("global_label") {
                if let Some(at) = elem.field("at") {
                    if at.len() >= 3 {
                        occupied_seed.push((cell(at.num(1), at.num(2)), Some(elem.str_at(1).to_string())));
                    }
                    if at.len() >= 4 {
                        labels.push((i, elem.str_at(1).to_string(), at.num(1), at.num(2), at.num(3)));
                    }
                }
            } else if elem.is("wire") {
                if let Some(pts) = elem.field("pts") {
                    for xy in pts.fields("xy") {
                        occupied_seed.push((cell(xy.num(1), xy.num(2)), None));
                    }
                }
            }
        }

        let (moves, new_wires) = self.deconflict_labels(&labels, &occupied_seed, node, sheet_tx);
        for (idx, nx, ny) in moves {
            if let Some(at) = elements[idx].field_mut("at").and_then(|a| a.list_mut()) {
                at[1] = Sexp::N(nx);
                at[2] = Sexp::N(ny);
            }
        }
        for (ax, ay, nx, ny) in new_wires {
            elements.push(wire_sexp(round_mm(ax), round_mm(ay), nx, ny, &format!("dcwire:{}:{}:{}:{}", ax, ay, nx, ny)));
        }
    }

    /// Spread net labels off part bodies once a sheet's elements are complete.
    pub fn deconflict_sheet_labels(&self, elements: &mut Vec<Sexp>, node: NodeId, tx: Tx) {
        self.apply_label_deconfliction(elements, node, tx);
        turn_crowded_power_values(elements);
    }

    /// Convert a node and the nodes below it to schematic files.
    ///
    /// A flattened node returns its elements for its parent's sheet. A node
    /// with a sheet of its own writes the sheet's file and returns the sheet
    /// symbol for its parent. Also returned are the parts whose library
    /// symbols a flattened node needs.
    fn node_to_sexp_schematic(
        &mut self,
        node: NodeId,
        uuid_path: &str,
        sheet_tx: Tx,
    ) -> Res<(Vec<Sexp>, IndexMap<String, PartId>, PathBuf)> {
        {
            let n = &mut self.nodes[node];
            let name = if n.sheet_filename.is_empty() { "no_sheet_filename".to_string() } else { n.sheet_filename.clone() };
            let stem = match name.rsplit_once('.') {
                Some((stem, ext)) if !stem.is_empty() && !ext.contains('/') => stem.to_string(),
                _ => name,
            };
            n.sheet_filename = stem + ".kicad_sch";
        }

        let flattened = self.nodes[node].flattened;
        let mut uuid_path = uuid_path.to_string();
        let (tx, paper) = if flattened {
            // A flattened node has no sheet of its own, so its elements go on its parent's.
            uuid_path = uuid_path.rsplit_once('/').map_or(String::new(), |(head, _)| head.to_string());
            (self.nodes[node].tx * sheet_tx, "")
        } else {
            calc_sheet_tx(&self.internal_bbox(node))
        };

        let mut elements: Vec<Sexp> = Vec::new();
        let mut lib_symbols: IndexMap<String, PartId> = IndexMap::new();
        for &part in &self.nodes[node].parts {
            let p = &self.c.parts[part];
            if !p.is_net_terminal {
                lib_symbols.insert(format!("{}:{}", p.lib, p.name), part);
            }
        }

        for (i, child) in self.children(node).into_iter().enumerate() {
            self.nodes[child].uuid = gen_uuid(&format!("{}_{}", self.nodes[child].name, i));
            let child_path = format!("{}/{}", uuid_path, self.nodes[child].uuid);
            let (sexps, parts, _) = self.node_to_sexp_schematic(child, &child_path, tx)?;
            elements.extend(sexps);
            lib_symbols.extend(parts);
        }

        // The children are done, so this node's own elements come next, on the sheet at uuid_path.
        self.kc.sheet_uuid_path = uuid_path.clone();
        let mut elements = self.sheet_connectivity(node, elements, tx, Some(&uuid_path));

        if flattened {
            return Ok((elements, lib_symbols, PathBuf::new()));
        }

        let mut schematic = sx![
            "kicad_sch",
            sx!["version", file_version(&self.opts.tool)],
            sx!["generator", "skidl"],
            sx!["generator_version", VERSION],
            sx!["uuid", self.nodes[node].uuid.as_str()],
            sx!["paper", paper]
        ];
        schematic.push(create_title_block_sexp(&self.nodes[node].title));

        let mut libs = sx!["lib_symbols"];
        for &part in lib_symbols.values() {
            libs.push(part_to_lib_symbol_definition(&self.c.parts[part], &self.opts.tool));
        }
        // Power-symbol definitions come from the power symbols actually on the
        // sheet, so every instance has one.
        let defined: HashSet<String> = lib_symbols.values().map(|&p| lib_id_of(&self.c.parts[p])).collect();
        for pwr_lib_id in power_lib_ids_in_elements(&elements) {
            if !lib_symbols.contains_key(&pwr_lib_id) && !defined.contains(&pwr_lib_id) {
                if let Some(sym) = extract_power_lib_symbol(&pwr_lib_id["power:".len()..]) {
                    libs.push(sym);
                }
            }
        }
        schematic.push(libs);

        self.deconflict_sheet_labels(&mut elements, node, tx);
        self.use_hierarchical_labels(node, &mut elements);
        schematic.list_mut().unwrap().extend(elements);

        let filepath = self.filepath.join(&self.nodes[node].sheet_filename);
        write_sexp_schematic(schematic, &filepath)?;

        // The parent gets a sheet symbol for this node.
        let sheet_uuid = uuid_path.rsplit('/').next().unwrap_or("").to_string();
        Ok((self.create_hierarchical_sheet_sexp(node, &sheet_uuid, sheet_tx), IndexMap::new(), filepath))
    }

    /// Write the schematic files of a placed and routed design. Returns the top file's path.
    pub fn write_top_schematic(&mut self) -> Res<PathBuf> {
        let root = self.root;
        let top = if self.top_name.is_empty() { "schematic".to_string() } else { self.top_name.clone() };
        self.kc = WriterState { project_name: top.clone(), ..Default::default() };
        self.nodes[root].title = self.title.clone();
        self.nodes[root].sheet_filename = top;
        // The top node has no parent to take its contents, so it always gets a sheet.
        self.nodes[root].flattened = false;

        if self.opts.place_fields {
            self.plan_field_layouts(root, true);
            self.kc.pwr_counter = 0; // Planning numbers power symbols, so start over.
        }

        // The top node's name is empty, so its UUID comes from the file name:
        // the same every run, but different between projects.
        let n = &self.nodes[root];
        let seed = if n.name.is_empty() { format!("root:{}", n.sheet_filename) } else { n.name.clone() };
        self.nodes[root].uuid = gen_uuid(&seed);
        let uuid_path = format!("/{}", self.nodes[root].uuid);
        let (_, _, output) = self.node_to_sexp_schematic(root, &uuid_path, Tx::IDENT)?;
        Ok(output)
    }
}

/// Turn the names of power symbols on neighboring pins upright so they don't run together.
///
/// A power symbol's name is wider than the 100 mils between two pins of an
/// IC, so the names of rails on pins side by side overlap each other and the
/// labels on the pins next to them. A name that would do so is turned to read
/// up the sheet, like the labels, where it's only one line wide.
pub fn turn_crowded_power_values(elements: &mut [Sexp]) {
    let labels: Vec<Box4> = ["global_label", "hierarchical_label"]
        .iter()
        .flat_map(|kind| elements.iter().filter(move |e| e.is(kind)))
        .map(q::label_box)
        .collect();

    // (element index, index of the Value field in it, its box).
    let mut values: Vec<(usize, usize, Box4)> = Vec::new();
    for (i, symbol) in elements.iter().enumerate() {
        if !symbol.is("symbol") || !symbol.field("lib_id").is_some_and(|l| l.str_at(1).starts_with("power:")) {
            continue;
        }
        let Some(fi) = symbol.list().unwrap().iter().position(|e| e.is("property") && e.str_at(1) == "Value") else {
            continue;
        };
        let value = &symbol.list().unwrap()[fi];
        if q::is_hidden(value) {
            continue;
        }
        values.push((i, fi, q::field_box(symbol, value, &InstanceMap::new(symbol))));
    }

    let mut boxes: Vec<Box4> = values.iter().map(|v| v.2).collect();
    let hits = |b: &Box4, k: usize, boxes: &[Box4]| -> usize {
        labels.iter().filter(|o| q::overlap(b, o)).count()
            + boxes.iter().enumerate().filter(|(j, o)| *j != k && q::overlap(b, o)).count()
    };

    // (index into values, upright box, new y, hits when level).
    let mut crowded: Vec<(usize, Box4, f64, usize)> = Vec::new();
    for (k, &(i, fi, b)) in values.iter().enumerate() {
        let symbol = &elements[i];
        let sat = symbol.field("at").unwrap();
        let (x, y, angle) = (sat.num(1), sat.num(2), sat.num(3));
        let value = &symbol.list().unwrap()[fi];
        let at = value.field("at").unwrap();
        // Only a name above or below its symbol can be turned to stand clear of it.
        let level_hits = hits(&b, k, &boxes);
        if angle.rem_euclid(180.0) != 0.0 || at.num(1) != x || at.num(2) == y || level_hits == 0 {
            continue;
        }
        let length = value.str_at(2).chars().count() as f64 * q::FONT_SIZE * q::CHAR_W;
        let height = q::FONT_SIZE * q::CHAR_H;
        let mid = round_mm(y + (POWER_VALUE_START_MM + length / 2.0).copysign(at.num(2) - y));
        let upright = (x - height / 2.0, mid - length / 2.0, x + height / 2.0, mid + length / 2.0);
        crowded.push((k, upright, mid, level_hits));
    }

    // Turn all the crowded names, so a row of them stays alike, then turn
    // back any that still overlap as much as they did (a label, say).
    for &(k, upright, _, _) in &crowded {
        boxes[k] = upright;
    }
    for &(k, upright, mid, level_hits) in &crowded {
        if hits(&upright, k, &boxes) >= level_hits {
            boxes[k] = values[k].2;
            continue;
        }
        let (i, fi, _) = values[k];
        let value = &mut elements[i].list_mut().unwrap()[fi];
        if let Some(at) = value.field_mut("at").and_then(|a| a.list_mut()) {
            at[2] = Sexp::N(mid);
            if at.len() > 3 {
                at[3] = Sexp::N(90.0);
            } else {
                at.push(Sexp::N(90.0));
            }
        }
        if let Some(effects) = value.field_mut("effects").and_then(|e| e.list_mut()) {
            effects.retain(|e| !e.is("justify"));
        }
    }
}
