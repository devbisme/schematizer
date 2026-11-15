//! Score the wiring and overlaps of generated schematics.
//!
//! Lower is better for every count:
//!
//! * `segments`: wire segments drawn
//! * `bends`: corners where exactly two perpendicular segments meet
//! * `crossings`: places where two wires cross without joining
//! * `wirelength`: total wire length (mm)
//! * `part_overlaps`: pairs of symbols whose drawings overlap
//! * `text_overlaps`: text items overlapping each other, another symbol or a wire

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::geometry::round_to;
use crate::sexp::Sexp;

// Text size as fractions of the font size, measured from KiCad 9's own plots.
pub const CHAR_W: f64 = 0.95;
pub const CHAR_H: f64 = 1.25;
pub const FONT_SIZE: f64 = 1.27; // mm.
pub const LABEL_H: f64 = 2.0; // Height of a global label's flag, mm.
pub const LABEL_END: f64 = 2.0; // Extra length of a global label's flag beyond its text, mm.
pub const TOUCH: f64 = 0.1; // Boxes must overlap by more than this, so touching pins don't count.

/// Wire length (mm) a bend, a crossing or an overlap is worth when comparing layouts.
pub const LAYOUT_BEND_COST: f64 = 5.0;
pub const LAYOUT_CROSSING_COST: f64 = 5.0;
pub const LAYOUT_OVERLAP_COST: f64 = 10.0;

/// A box as (x1, y1, x2, y2).
pub type Box4 = (f64, f64, f64, f64);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Score {
    pub segments: usize,
    pub bends: usize,
    pub crossings: usize,
    pub wirelength: f64,
    pub part_overlaps: usize,
    pub text_overlaps: usize,
}

impl Score {
    pub fn add(&mut self, o: &Score) {
        self.segments += o.segments;
        self.bends += o.bends;
        self.crossings += o.crossings;
        self.wirelength += o.wirelength;
        self.part_overlaps += o.part_overlaps;
        self.text_overlaps += o.text_overlaps;
    }
}

/// Combine the scores of a layout into one number.
pub fn layout_cost(score: &Score) -> f64 {
    let overlaps = (score.part_overlaps + score.text_overlaps) as f64;
    score.wirelength
        + LAYOUT_BEND_COST * score.bends as f64
        + LAYOUT_CROSSING_COST * score.crossings as f64
        + LAYOUT_OVERLAP_COST * overlaps
}

fn key(x: f64, y: f64) -> (i64, i64) {
    ((x * 100.0).round() as i64, (y * 100.0).round() as i64)
}

/// Count the places where a horizontal and a vertical segment cross.
///
/// Where one segment ends on another they meet (a corner or a tee) and don't
/// cross. Crossings at the given junction points aren't counted.
pub fn count_crossings(segs: &[((f64, f64), (f64, f64))], junctions: &HashSet<(i64, i64)>) -> usize {
    let mut horz = Vec::new();
    let mut vert = Vec::new();
    for (a, b) in segs {
        if a.1 == b.1 {
            horz.push((a.0.min(b.0), a.0.max(b.0), a.1));
        }
        if a.0 == b.0 {
            vert.push((a.1.min(b.1), a.1.max(b.1), a.0));
        }
    }
    let mut n = 0;
    for &(x1, x2, y) in &horz {
        for &(y1, y2, x) in &vert {
            if x1 < x && x < x2 && y1 < y && y < y2 && !junctions.contains(&key(x, y)) {
                n += 1;
            }
        }
    }
    n
}

type Seg = ((f64, f64), (f64, f64));

/// Score wire segments and junction points.
pub fn score_wires(wires: &[Seg], junctions: &HashSet<(i64, i64)>) -> Score {
    let segs: Vec<Seg> = wires.iter().copied().filter(|s| s.0 != s.1).collect();
    let mut ends: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    for (i, (p1, p2)) in segs.iter().enumerate() {
        ends.entry(key(p1.0, p1.1)).or_default().push(i);
        ends.entry(key(p2.0, p2.1)).or_default().push(i);
    }
    let vertical = |i: usize| segs[i].0 .0 == segs[i].1 .0;
    let bends = ends.values().filter(|m| m.len() == 2 && vertical(m[0]) != vertical(m[1])).count();
    let length: f64 = segs.iter().map(|(p1, p2)| (p1.0 - p2.0).abs() + (p1.1 - p2.1).abs()).sum();
    Score {
        segments: segs.len(),
        bends,
        crossings: count_crossings(&segs, junctions),
        wirelength: round_to(length, 2),
        ..Default::default()
    }
}

fn r2(v: f64) -> f64 {
    round_to(v, 2)
}

fn sch_wires(sch: &Sexp) -> Vec<Seg> {
    let mut wires = Vec::new();
    for w in sch.fields("wire") {
        if let Some(pts) = w.field("pts") {
            let xy: Vec<&Sexp> = pts.fields("xy").collect();
            if xy.len() >= 2 {
                wires.push(((r2(xy[0].num(1)), r2(xy[0].num(2))), (r2(xy[1].num(1)), r2(xy[1].num(2)))));
            }
        }
    }
    wires
}

fn svg_attr(line: &str, name: &str) -> Option<f64> {
    let pat = format!("{}=\"", name);
    let start = line.find(&pat)? + pat.len();
    let end = line[start..].find('"')? + start;
    line[start..end].parse().ok()
}

fn svg_wires(text: &str) -> Vec<Seg> {
    let mut wires = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("<line x1=\"") {
        let tag = &rest[i..];
        let end = tag.find('>').unwrap_or(tag.len());
        let tag_text = &tag[..end];
        if let (Some(x1), Some(y1), Some(x2), Some(y2)) =
            (svg_attr(tag_text, "x1"), svg_attr(tag_text, "y1"), svg_attr(tag_text, "x2"), svg_attr(tag_text, "y2"))
        {
            wires.push(((r2(x1), r2(y1)), (r2(x2), r2(y2))));
        }
        rest = &tag[end..];
    }
    wires
}

/// Score the text of one schematic file (.kicad_sch or .svg).
pub fn score_text(text: &str) -> Score {
    if text.trim_start().starts_with("(kicad_sch") {
        if let Some(sch) = Sexp::parse(text) {
            let wires = sch_wires(&sch);
            let junctions: HashSet<(i64, i64)> =
                sch.fields("junction").filter_map(|j| j.field("at")).map(|a| key(r2(a.num(1)), r2(a.num(2)))).collect();
            let mut score = score_wires(&wires, &junctions);
            let (parts, texts) = score_overlaps(&sch, &wires);
            score.part_overlaps = parts;
            score.text_overlaps = texts;
            return score;
        }
    }
    score_wires(&svg_wires(text), &HashSet::new())
}

/// Library-coordinate points outlining one unit of a symbol definition.
pub fn symbol_points(lib_symbol: &Sexp, unit: i64) -> Vec<(f64, f64)> {
    let mut pts = Vec::new();
    let show_names = match lib_symbol.field("pin_names") {
        Some(pn) => !(pn.has_atom("hide") || pn.field("hide").is_some()),
        None => true,
    };
    let unit_str = unit.to_string();
    for sub in lib_symbol.fields("symbol") {
        // Sub-symbols are named NAME_UNIT_STYLE; unit 0 is common to all units.
        let name = sub.str_at(1);
        let pieces: Vec<&str> = name.rsplitn(3, '_').collect();
        if pieces.len() < 3 || !(pieces[1] == "0" || pieces[1] == unit_str) {
            continue;
        }
        for item in sub.list().into_iter().flatten() {
            match item.head() {
                Some("polyline") | Some("bezier") => {
                    if let Some(p) = item.field("pts") {
                        pts.extend(p.fields("xy").map(|xy| (xy.num(1), xy.num(2))));
                    }
                }
                Some("rectangle") | Some("arc") => {
                    for head in ["start", "mid", "end"] {
                        if let Some(at) = item.field(head) {
                            pts.push((at.num(1), at.num(2)));
                        }
                    }
                }
                Some("circle") => {
                    if let (Some(c), Some(r)) = (item.field("center"), item.field("radius")) {
                        let (x, y, radius) = (c.num(1), c.num(2), r.num(1));
                        pts.push((x - radius, y - radius));
                        pts.push((x + radius, y + radius));
                    }
                }
                Some("pin") => {
                    let Some(at) = item.field("at") else { continue };
                    let (x, y, angle) = (at.num(1), at.num(2), at.num(3));
                    let length = item.field("length").map_or(0.0, |l| l.num(1));
                    let (dx, dy) = match (angle as i64).rem_euclid(360) {
                        90 => (0.0, 1.0),
                        180 => (-1.0, 0.0),
                        270 => (0.0, -1.0),
                        _ => (1.0, 0.0),
                    };
                    pts.push((x, y));
                    pts.push((x + dx * length, y + dy * length));
                    if show_names && item.field("name").is_some_and(|n| n.len() > 1 && n.str_at(1) != "~") {
                        // A pin's name is drawn alongside it, on either side.
                        let (pad_x, pad_y) = (dy * FONT_SIZE, dx * FONT_SIZE);
                        pts.push((x + pad_x, y + pad_y));
                        pts.push((x - pad_x, y - pad_y));
                    }
                }
                _ => {}
            }
        }
    }
    pts
}

/// Maps a symbol's library coordinates onto its sheet.
#[derive(Clone, Copy, Debug)]
pub struct InstanceMap {
    ox: f64,
    oy: f64,
    cos: f64,
    sin: f64,
    mirror: char,
}

impl InstanceMap {
    pub fn new(symbol: &Sexp) -> InstanceMap {
        let at = symbol.field("at");
        let (ox, oy, angle) = at.map_or((0.0, 0.0, 0.0), |a| (a.num(1), a.num(2), a.num(3)));
        let mirror = symbol.field("mirror").map_or(' ', |m| m.str_at(1).chars().next().unwrap_or(' '));
        let (cos, sin) = match (angle as i64).rem_euclid(360) {
            90 => (0.0, 1.0),
            180 => (-1.0, 0.0),
            270 => (0.0, -1.0),
            _ => (1.0, 0.0),
        };
        InstanceMap { ox, oy, cos, sin, mirror }
    }

    pub fn map(&self, x: f64, y: f64) -> (f64, f64) {
        let y = -y; // Library coordinates are Y-up, sheets are Y-down.
        let (mut x, mut y) = (x * self.cos + y * self.sin, -x * self.sin + y * self.cos);
        if self.mirror == 'x' {
            y = -y;
        } else if self.mirror == 'y' {
            x = -x;
        }
        (self.ox + x, self.oy + y)
    }
}

pub fn overlap(b1: &Box4, b2: &Box4) -> bool {
    b1.0 + TOUCH < b2.2 && b2.0 + TOUCH < b1.2 && b1.1 + TOUCH < b2.3 && b2.1 + TOUCH < b1.3
}

/// True if a horizontal or vertical wire runs through a box.
pub fn crosses(b: &Box4, wire: &Box4) -> bool {
    wire.0 < b.2 - TOUCH && b.0 + TOUCH < wire.2 + TOUCH * 2.0 && wire.1 < b.3 - TOUCH && b.1 + TOUCH < wire.3 + TOUCH * 2.0
}

/// True if a field (or other element) is hidden by its effects.
pub fn is_hidden(field: &Sexp) -> bool {
    field.field("effects").is_some_and(|e| e.field("hide").is_some())
}

/// Sheet box of a visible Reference/Value field.
pub fn field_box(symbol: &Sexp, field: &Sexp, to_sheet: &InstanceMap) -> Box4 {
    let at = field.field("at");
    let (x, y, angle) = at.map_or((0.0, 0.0, 0.0), |a| (a.num(1), a.num(2), a.num(3)));
    let w = field.get(2).map_or(0, |t| t.text().chars().count()) as f64 * FONT_SIZE * CHAR_W;
    let h = FONT_SIZE * CHAR_H;
    let mut justify = field
        .field("effects")
        .and_then(|e| e.field("justify"))
        .and_then(|j| j.list().unwrap().iter().skip(1).filter_map(|e| e.as_str()).find(|s| *s == "left" || *s == "right"));

    // KiCad turns a field with its symbol, so a field at 90 degrees on an
    // upright symbol reads up the sheet. (Only centered ones are written.)
    let sym_angle = symbol.field("at").map_or(0.0, |a| a.num(3));
    if ((angle + sym_angle) as i64).rem_euclid(180) != 0 {
        return (x - h / 2.0, y - w / 2.0, x + h / 2.0, y + w / 2.0);
    }

    // The justification is relative to the symbol: see if the text's reading
    // direction comes out reversed on the sheet.
    let origin = to_sheet.map(0.0, 0.0);
    let tip = if (angle as i64).rem_euclid(180) == 0 { to_sheet.map(1.0, 0.0) } else { to_sheet.map(0.0, 1.0) };
    if tip.0 < origin.0 {
        justify = justify.map(|j| if j == "right" { "left" } else { "right" });
    }
    match justify {
        Some("left") => (x, y - h / 2.0, x + w, y + h / 2.0),
        Some("right") => (x - w, y - h / 2.0, x, y + h / 2.0),
        _ => (x - w / 2.0, y - h / 2.0, x + w / 2.0, y + h / 2.0),
    }
}

/// Sheet box of a global label's flag, which extends away from its anchor.
pub fn label_box(label: &Sexp) -> Box4 {
    let at = label.field("at");
    let (x, y, angle) = at.map_or((0.0, 0.0, 0.0), |a| (a.num(1), a.num(2), a.num(3)));
    let length = label.get(1).map_or(0, |t| t.text().chars().count()) as f64 * FONT_SIZE * CHAR_W + LABEL_END;
    let half = LABEL_H / 2.0;
    match (angle as i64).rem_euclid(360) {
        180 => (x - length, y - half, x, y + half),
        90 => (x - half, y - length, x + half, y),
        270 => (x - half, y, x + half, y + length),
        _ => (x, y - half, x + length, y + half),
    }
}

/// Count the overlapping symbols and text of a parsed .kicad_sch file.
///
/// Returns (part overlaps, text overlaps). Text extents are estimates.
pub fn score_overlaps(sch: &Sexp, wires: &[Seg]) -> (usize, usize) {
    let mut lib_symbols: HashMap<&str, &Sexp> = HashMap::new();
    if let Some(libs) = sch.field("lib_symbols") {
        for sym in libs.fields("symbol") {
            lib_symbols.insert(sym.str_at(1), sym);
        }
    }

    let mut bodies: Vec<Box4> = Vec::new();
    // Text is stored with its symbol's index, or None for a label.
    let mut texts: Vec<(Box4, Option<usize>)> = Vec::new();
    for kind in ["global_label", "hierarchical_label"] {
        for label in sch.fields(kind) {
            texts.push((label_box(label), None));
        }
    }
    for symbol in sch.fields("symbol") {
        let Some(lib_id) = symbol.field("lib_id") else { continue };
        let Some(lib_symbol) = lib_symbols.get(lib_id.str_at(1)) else { continue };
        let unit = symbol.field("unit").map_or(1, |u| u.num(1) as i64);
        let to_sheet = InstanceMap::new(symbol);
        let pts: Vec<(f64, f64)> = symbol_points(lib_symbol, unit).iter().map(|&(x, y)| to_sheet.map(x, y)).collect();
        if pts.is_empty() {
            continue;
        }
        let index = bodies.len();
        bodies.push(bounds(&pts));
        for field in symbol.fields("property") {
            let name = field.str_at(1);
            if (name == "Reference" || name == "Value")
                && field.get(2).is_some_and(|t| !t.text().is_empty())
                && !is_hidden(field)
            {
                texts.push((field_box(symbol, field, &to_sheet), Some(index)));
            }
        }
    }

    let mut part_overlaps = 0;
    for (i, b1) in bodies.iter().enumerate() {
        part_overlaps += bodies[i + 1..].iter().filter(|b2| overlap(b1, b2)).count();
    }
    // Text sits against its own symbol by design, so only other symbols count.
    let mut text_overlaps = 0;
    for (b, owner) in &texts {
        text_overlaps += bodies.iter().enumerate().filter(|(i, body)| Some(*i) != *owner && overlap(b, body)).count();
    }
    for (i, (b1, _)) in texts.iter().enumerate() {
        text_overlaps += texts[i + 1..].iter().filter(|(b2, _)| overlap(b1, b2)).count();
    }
    // Labels are attached to wires, so only a part's text can collide with one.
    let wire_boxes: Vec<Box4> =
        wires.iter().map(|(a, b)| (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))).collect();
    for (b, owner) in &texts {
        if owner.is_some() {
            text_overlaps += wire_boxes.iter().filter(|w| crosses(b, w)).count();
        }
    }
    (part_overlaps, text_overlaps)
}

/// The box around some points.
pub fn bounds(pts: &[(f64, f64)]) -> Box4 {
    let mut b = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for &(x, y) in pts {
        b = (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y));
    }
    b
}

/// Score and total the schematic files in a file or a directory.
///
/// A directory's .svg files are only scored if it has no .kicad_sch files,
/// since both formats draw the same wires.
pub fn score_files(path: &Path) -> Score {
    let mut total = Score::default();
    let mut files = Vec::new();
    if path.is_dir() {
        for ext in ["kicad_sch", "svg"] {
            let mut found: Vec<_> = std::fs::read_dir(path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == ext))
                .collect();
            found.sort();
            if !found.is_empty() {
                files = found;
                break;
            }
        }
    } else {
        files.push(path.to_path_buf());
    }
    for f in files {
        if let Ok(text) = std::fs::read_to_string(&f) {
            total.add(&score_text(&text));
        }
    }
    total.wirelength = round_to(total.wirelength, 2);
    total
}
