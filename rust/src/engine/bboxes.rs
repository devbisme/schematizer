//! Bounding boxes of part symbols and hierarchical labels.

use crate::geometry::{BBox, Point, Tx};
use crate::ir::{Circuit, PartId};
use crate::sexp::Sexp;

use super::constants::{HIER_TERM_SIZE, PIN_LABEL_FONT_SIZE};

const CHAR_SIZE_FUDGE: f64 = 0.6;
const MM_TO_MILS: f64 = 1.0 / 0.0254;

fn xy(e: &Sexp) -> Point {
    Point::new(e.num(1), e.num(2))
}

fn font_size(effects: Option<&Sexp>) -> (f64, f64) {
    effects
        .and_then(|e| e.field("font"))
        .and_then(|f| f.field("size"))
        .map_or((0.0, 0.0), |s| (s.num(1), s.num(2)))
}

fn text_bbox(text: &str, start: Point, dir: Point, char_wid: f64, char_hgt: f64) -> BBox {
    if text.is_empty() {
        return BBox::empty();
    }
    let char_wid = char_wid * CHAR_SIZE_FUDGE;
    let char_hgt = char_hgt * CHAR_SIZE_FUDGE;
    let ortho = dir * Tx::IDENT.rot(90.0);
    let n = text.chars().count() as f64;
    let p1 = start - ortho * char_hgt / 2.0;
    let p2 = start + ortho * char_hgt / 2.0;
    let p3 = p1 + dir * char_wid * n;
    let p4 = p2 + dir * char_wid * n;
    BBox::from_pts(&[p1, p2, p3, p4])
}

/// Bounding box (symbol mm) of one drawing command of a symbol.
fn draw_cmd_bbox(cmd: &Sexp) -> BBox {
    if cmd.len() < 2 {
        return BBox::empty();
    }
    let kind = cmd.head().unwrap_or("").to_lowercase();
    match kind.as_str() {
        "polyline" | "bezier" => {
            let pts: Vec<Point> = cmd.field("pts").map(|p| p.fields("xy").map(xy).collect()).unwrap_or_default();
            if kind == "bezier" && pts.len() < 4 {
                return BBox::empty();
            }
            BBox::from_pts(&pts)
        }
        "circle" => {
            let ctr = cmd.field("center").map_or(Point::new(0.0, 0.0), xy);
            let radius = cmd.field("radius").map_or(0.0, |r| r.num(1));
            let rad = Point::new(radius, radius);
            BBox::from_pts(&[ctr + rad, ctr - rad])
        }
        "rectangle" => {
            let start = cmd.field("start").map_or(Point::new(0.0, 0.0), xy);
            let end = cmd.field("end").map_or(Point::new(0.0, 0.0), xy);
            BBox::new(start, end)
        }
        "arc" => {
            let pts: Vec<Point> = ["start", "end", "mid"]
                .iter()
                .map(|h| cmd.field(h).map_or(Point::new(0.0, 0.0), xy))
                .collect();
            BBox::from_pts(&pts)
        }
        "text" => {
            let effects = cmd.field("effects");
            if effects.is_some_and(|e| e.field("hide").is_some()) {
                return BBox::empty();
            }
            let at = cmd.field("at");
            let start = at.map_or(Point::new(0.0, 0.0), xy);
            let rotation = at.map_or(0.0, |a| a.num(3));
            let size = font_size(effects);
            let text = cmd.list().unwrap().iter().skip(1).find_map(|e| e.as_str()).unwrap_or("");
            let dir = Point::new(-1.0, 0.0) * Tx::IDENT.rot(rotation);
            text_bbox(text, start, dir, size.0 * CHAR_SIZE_FUDGE, size.1 * CHAR_SIZE_FUDGE)
        }
        "property" => {
            let effects = cmd.field("effects");
            if effects.is_some_and(|e| e.field("hide").is_some()) {
                return BBox::empty();
            }
            let at = cmd.field("at");
            let start = at.map_or(Point::new(0.0, 0.0), xy);
            let rotation = at.map_or(0.0, |a| a.num(3));
            let size = font_size(effects);
            // The bare items of a property are its name and its value.
            let bare: Vec<String> =
                cmd.list().unwrap().iter().skip(1).filter(|e| !e.is_list()).map(|e| e.text()).collect();
            let text = if bare.len() > 1 { bare[1].clone() } else { String::new() };
            let justify = effects
                .and_then(|e| e.field("justify"))
                .map_or("center".to_string(), |j| j.str_at(1).to_lowercase());
            let dir = match justify.as_str() {
                "left" => Point::new(1.0, 0.0),
                _ => Point::new(-1.0, 0.0),
            } * Tx::IDENT.rot(rotation);
            text_bbox(&text, start, dir, size.0 * CHAR_SIZE_FUDGE, size.1 * CHAR_SIZE_FUDGE)
        }
        "pin" => {
            let at = cmd.field("at");
            let start = at.map_or(Point::new(0.0, 0.0), xy);
            let rotation = at.map_or(0.0, |a| a.num(3));
            let length = cmd.field("length").map_or(0.0, |l| l.num(1));
            let dir = Point::new(1.0, 0.0) * Tx::IDENT.rot(rotation);
            BBox::from_pts(&[start, start + dir * length])
        }
        _ => BBox::empty(),
    }
}

fn to_mils(b: BBox) -> BBox {
    if b.is_empty() {
        // Python's infinities come out of the scaling as an unbounded box.
        return BBox { min: Point::new(f64::NEG_INFINITY, f64::NEG_INFINITY), max: Point::new(f64::INFINITY, f64::INFINITY) };
    }
    b * Tx::scale_by(MM_TO_MILS)
}

/// Bounding boxes (mils) of the symbols of a part's units (or of the part, if it has none).
pub fn calc_symbol_bbox(c: &Circuit, part: PartId) -> Vec<BBox> {
    let p = &c.parts[part];
    let empty = Vec::new();
    let cmds = |key: i64| p.draw_cmds.get(&key).unwrap_or(&empty);
    let mut bboxes = Vec::new();
    if p.units.is_empty() {
        let mut bbox = BBox::empty();
        for key in [1, 0] {
            for cmd in cmds(key) {
                bbox.add(draw_cmd_bbox(cmd));
            }
        }
        bboxes.push(to_mils(bbox));
    } else {
        for &unit in &p.units {
            let num = c.parts[unit].num;
            let mut bbox = BBox::empty();
            for key in [num, 0] {
                for cmd in cmds(key) {
                    bbox.add(draw_cmd_bbox(cmd));
                }
            }
            bboxes.push(to_mils(bbox));
        }
    }
    bboxes
}

/// Bounding box (mils) of a hierarchical label and its terminal.
pub fn calc_hier_label_bbox(label: &str, dir: char) -> BBox {
    let tx = match dir {
        'U' => Tx::ROT_90,
        'D' => Tx::ROT_270,
        'L' => Tx::ROT_180,
        _ => Tx::ROT_0,
    };
    let lbl_len = label.chars().count() as f64 * PIN_LABEL_FONT_SIZE + HIER_TERM_SIZE;
    let lbl_hgt = PIN_LABEL_FONT_SIZE.max(HIER_TERM_SIZE);
    BBox::new(Point::new(0.0, lbl_hgt / 2.0), Point::new(-lbl_len, -lbl_hgt / 2.0)) * tx
}
